//! The typed error surface of the shared-memory plane.
//!
//! Every failure mode a caller can reasonably branch on is its own variant.
//! That matters more here than in most crates because the data plane's
//! recovery policies are written *against* these variants, not against
//! strings:
//!
//! | Variant | Blueprint policy |
//! |---|---|
//! | [`ShmError::PoolExhausted`] | §6.2 — never sleep-retry; fall back to the reliable daemon path and bump `shm_fallback_total` |
//! | [`ShmError::StaleGeneration`] | §6.2 — a restarted node gets a new generation; every reader detects the stale mapping |
//! | [`ShmError::Closed`] | §6.2 — the daemon marked the segment closed on producer death; drain, then detach |
//! | [`ShmError::Unsupported`] | Windows (and any non-Unix target): the plane is compile-gated, callers degrade to UDS/QUIC |
//! | [`RecvError::Lagged`] | SPMC overwrite policy — the consumer fell behind and messages were overwritten |
//!
//! # Examples
//!
//! ```
//! use astrs_shm::{ShmError, ShmResult};
//!
//! fn classify(err: &ShmError) -> &'static str {
//!     match err {
//!         ShmError::PoolExhausted { .. } => "fall back to the daemon path",
//!         ShmError::StaleGeneration { .. } => "re-attach with the new generation",
//!         ShmError::Closed { .. } => "drain and detach",
//!         _ => "propagate",
//!     }
//! }
//!
//! let err = ShmError::PoolExhausted { slot_count: 8 };
//! assert_eq!(classify(&err), "fall back to the daemon path");
//! # let _: fn(&ShmError) -> &'static str = classify;
//! # Ok::<(), ShmError>(())
//! # ;
//! # let _unused: ShmResult<()> = Ok(());
//! ```

use std::fmt;
use std::io;

/// The result type used throughout this crate.
pub type ShmResult<T> = Result<T, ShmError>;

/// Everything that can go wrong on the same-host zero-copy plane.
///
/// The variant set is deliberately wide: a caller that wants to distinguish
/// "the pool is momentarily full" (retry elsewhere, immediately) from "this
/// mapping belongs to a dead incarnation of the node" (re-resolve the route)
/// must not have to parse a message.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ShmError {
    /// The platform has no shared-memory plane implementation.
    ///
    /// Returned by every constructor on non-Unix targets. Callers degrade to
    /// the reliable daemon path (UDS) or the cross-host plane.
    #[error("the shared-memory plane is not supported on this platform ({operation})")]
    Unsupported {
        /// The operation that was attempted, for diagnostics.
        operation: &'static str,
    },

    /// A syscall failed.
    #[error("{operation} failed: {source}")]
    Os {
        /// The syscall or high-level operation that failed.
        operation: &'static str,
        /// The underlying OS error.
        #[source]
        source: io::Error,
    },

    /// The mapped bytes are not an AstRS segment: the magic did not match.
    #[error("segment magic mismatch: expected {expected:?}, found {found:?}")]
    BadMagic {
        /// The magic this build writes.
        expected: [u8; 4],
        /// What was actually found at offset 0.
        found: [u8; 4],
    },

    /// The segment was written by an incompatible layout version.
    ///
    /// The segment layout is versioned independently of the wire protocol
    /// because a segment outlives no process boundary negotiation: it is
    /// mapped, not handshaken.
    #[error("segment layout version {found} is not supported (this build speaks {supported})")]
    LayoutVersion {
        /// The version stamped in the mapped header.
        found: u16,
        /// The version this build implements.
        supported: u16,
    },

    /// The header's self-described geometry is inconsistent or does not fit
    /// the mapping.
    ///
    /// This is the guard that turns a corrupt or hostile segment into an
    /// error instead of an out-of-bounds pointer. See
    /// [`crate::layout::SegmentLayout::validate_header`].
    #[error("segment header is corrupt: {reason}")]
    CorruptHeader {
        /// A precise description of the inconsistency.
        reason: CorruptReason,
    },

    /// The mapping belongs to a different incarnation of the producer.
    ///
    /// Blueprint §6.2: a restarted node gets a new generation, so every stale
    /// mapping is detectable by every reader. All attach paths verify this.
    #[error("stale segment generation: expected {expected}, mapped segment is generation {found}")]
    StaleGeneration {
        /// The generation the caller was told to expect.
        expected: u64,
        /// The generation actually stamped in the segment header.
        found: u64,
    },

    /// The mapped segment does not carry the key the caller asked for.
    ///
    /// On macOS the OS-visible name is a truncated hash of the logical key
    /// (name-length limits, §23 risk #7), so a collision is possible in
    /// principle. The header carries the full 128-bit key digest and every
    /// attach verifies it, turning a collision into this error rather than
    /// silent cross-wiring between two dataflows.
    #[error(
        "segment key digest mismatch: expected {expected:032x}, mapped segment carries {found:032x}"
    )]
    KeyMismatch {
        /// The digest of the key the caller asked for.
        expected: u128,
        /// The digest stamped in the mapped header.
        found: u128,
    },

    /// The segment has been marked closed and no longer accepts writers.
    ///
    /// Consumers may still drain whatever is READY; producers get this from
    /// every allocate.
    #[error("segment {name} is closed")]
    Closed {
        /// The segment's OS-visible (or synthetic) name, for diagnostics.
        name: String,
    },

    /// No slot could be reclaimed for a new message.
    ///
    /// Blueprint §6.2 (the dora PR-2366 lesson): **never sleep-retry.** The
    /// caller falls back to the reliable daemon path and increments
    /// `shm_fallback_total`.
    #[error("shared-memory pool exhausted: all {slot_count} slots are unreclaimable")]
    PoolExhausted {
        /// How many slots the ring has.
        slot_count: u32,
    },

    /// The requested payload does not fit a slot.
    #[error("payload of {requested} bytes exceeds the {capacity}-byte slot payload capacity")]
    PayloadTooLarge {
        /// The requested length.
        requested: usize,
        /// The per-slot payload capacity.
        capacity: usize,
    },

    /// The metadata blob does not fit the slot's metadata region.
    #[error("metadata of {requested} bytes exceeds the {capacity}-byte slot metadata capacity")]
    MetadataTooLarge {
        /// The requested metadata length.
        requested: usize,
        /// The per-slot metadata capacity.
        capacity: usize,
    },

    /// The consumer table is full: no free entry to register in.
    ///
    /// The table is sized at segment creation
    /// ([`crate::SegmentConfig::max_consumers`]); a dataflow whose fan-out
    /// grew past that needs a larger pool, not a retry.
    #[error(
        "consumer table is full ({max_consumers} entries); segment cannot accept another reader"
    )]
    ConsumerTableFull {
        /// The configured capacity of the consumer table.
        max_consumers: u32,
    },

    /// A configuration value is outside the range the layout can express.
    #[error("invalid segment configuration: {reason}")]
    InvalidConfig {
        /// What was wrong.
        reason: String,
    },

    /// A segment name (or a key component) is not usable on this platform.
    #[error("invalid segment name: {reason}")]
    InvalidName {
        /// What was wrong.
        reason: String,
    },

    /// The segment has no OS-visible name, so it cannot be opened by name.
    ///
    /// Linux `memfd` segments are anonymous: the only way in is the file
    /// descriptor, brokered by the daemon
    /// ([`crate::SegmentBroker`]).
    #[error("segment is anonymous (memfd-backed) and can only be attached to by file descriptor")]
    Anonymous,

    /// The producer process is gone.
    ///
    /// Raised by the consumer-side liveness check when the ring goes quiet
    /// and [`crate::SegmentHeaderView::producer_pid`] no longer names a live
    /// process — the case where the daemon died too and nobody ever set the
    /// `closed` flag.
    #[error("producer process {pid} is no longer alive")]
    ProducerGone {
        /// The pid recorded in the segment header.
        pid: i64,
    },

    /// A broker protocol message was malformed.
    #[error("broker protocol error: {reason}")]
    Protocol {
        /// What was wrong with the message.
        reason: String,
    },

    /// The broker refused a request.
    #[error("broker refused the request: {reason}")]
    BrokerRefused {
        /// The refusal reason as reported by the broker.
        reason: String,
    },

    /// A blocking wait hit its deadline.
    #[error("timed out after {}ms waiting for {operation}", timeout_ms)]
    Timeout {
        /// What was being waited for.
        operation: &'static str,
        /// The elapsed budget in milliseconds.
        timeout_ms: u64,
    },
}

impl ShmError {
    /// Build an [`ShmError::Os`] from a `rustix` errno.
    pub(crate) fn os(operation: &'static str, errno: rustix::io::Errno) -> Self {
        Self::Os {
            operation,
            source: io::Error::from_raw_os_error(errno.raw_os_error()),
        }
    }

    /// Build an [`ShmError::Os`] from a `std::io::Error`.
    pub(crate) fn io(operation: &'static str, source: io::Error) -> Self {
        Self::Os { operation, source }
    }

    /// Build an [`ShmError::Unsupported`].
    #[must_use]
    pub const fn unsupported(operation: &'static str) -> Self {
        Self::Unsupported { operation }
    }

    /// Build an [`ShmError::InvalidConfig`].
    pub(crate) fn invalid_config(reason: impl Into<String>) -> Self {
        Self::InvalidConfig {
            reason: reason.into(),
        }
    }

    /// Build an [`ShmError::Protocol`].
    ///
    /// Called only from `crate::fdpass` and `crate::protocol`, both
    /// `#[cfg(unix)]`-only — dead code on a Windows build otherwise.
    #[cfg(unix)]
    pub(crate) fn protocol(reason: impl Into<String>) -> Self {
        Self::Protocol {
            reason: reason.into(),
        }
    }

    /// Whether the caller should fall back to the reliable daemon path
    /// instead of retrying on this plane.
    ///
    /// Blueprint §6.2 makes this a policy, not a heuristic: pool exhaustion
    /// and a closed segment both mean "this route cannot carry the message
    /// right now"; sleeping and retrying is exactly the failure mode the
    /// design forbids.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::ShmError;
    ///
    /// assert!(ShmError::PoolExhausted { slot_count: 4 }.should_fall_back());
    /// assert!(ShmError::unsupported("create").should_fall_back());
    /// assert!(!ShmError::Anonymous.should_fall_back());
    /// ```
    #[must_use]
    pub const fn should_fall_back(&self) -> bool {
        matches!(
            self,
            Self::PoolExhausted { .. }
                | Self::Closed { .. }
                | Self::Unsupported { .. }
                | Self::ProducerGone { .. }
                | Self::StaleGeneration { .. }
        )
    }

    /// Whether this error means the mapping must be torn down and re-resolved.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::ShmError;
    ///
    /// assert!(ShmError::StaleGeneration { expected: 2, found: 1 }.is_fatal_for_mapping());
    /// assert!(!ShmError::PoolExhausted { slot_count: 4 }.is_fatal_for_mapping());
    /// ```
    #[must_use]
    pub const fn is_fatal_for_mapping(&self) -> bool {
        matches!(
            self,
            Self::StaleGeneration { .. }
                | Self::KeyMismatch { .. }
                | Self::BadMagic { .. }
                | Self::LayoutVersion { .. }
                | Self::CorruptHeader { .. }
                | Self::Closed { .. }
                | Self::ProducerGone { .. }
        )
    }
}

impl From<rustix::io::Errno> for ShmError {
    fn from(errno: rustix::io::Errno) -> Self {
        Self::os("syscall", errno)
    }
}

/// Precisely why a mapped header failed validation.
///
/// Every variant is a distinct check in
/// [`crate::layout::SegmentLayout::validate_header`]; keeping them apart
/// makes a corruption report actionable instead of "the header is bad".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CorruptReason {
    /// The mapping is smaller than a header.
    MappingTooSmall {
        /// How many bytes were mapped.
        mapped: usize,
        /// The minimum the layout requires.
        required: usize,
    },
    /// `header_len` does not equal the constant this layout version fixes.
    HeaderLen {
        /// The value found in the mapping.
        found: u16,
        /// The value this layout version fixes.
        expected: u16,
    },
    /// A geometry field is zero where the layout forbids it.
    ZeroGeometry {
        /// Which field.
        field: &'static str,
    },
    /// A geometry field exceeds the hard cap the layout enforces.
    GeometryTooLarge {
        /// Which field.
        field: &'static str,
        /// The value found.
        found: u64,
        /// The cap.
        limit: u64,
    },
    /// The recorded geometry overflows 64-bit arithmetic.
    ///
    /// This is the check that makes a `slot_count = u32::MAX` header an error
    /// rather than a wild pointer.
    GeometryOverflow {
        /// Which product overflowed.
        field: &'static str,
    },
    /// A recorded region offset disagrees with the offset recomputed from the
    /// geometry.
    OffsetMismatch {
        /// Which region.
        region: &'static str,
        /// The offset recorded in the header.
        recorded: u64,
        /// The offset recomputed from `(slot_count, capacities, …)`.
        computed: u64,
    },
    /// A region is not aligned as the layout requires.
    Misaligned {
        /// Which region.
        region: &'static str,
        /// The offending offset.
        offset: u64,
        /// The required alignment.
        align: u64,
    },
    /// The described segment does not fit the mapping.
    TotalLenMismatch {
        /// The total length recorded / computed.
        described: u64,
        /// The number of bytes actually mapped.
        mapped: u64,
    },
}

impl fmt::Display for CorruptReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MappingTooSmall { mapped, required } => write!(
                f,
                "mapping of {mapped} bytes is smaller than the {required}-byte minimum"
            ),
            Self::HeaderLen { found, expected } => {
                write!(f, "header_len is {found}, expected {expected}")
            }
            Self::ZeroGeometry { field } => write!(f, "{field} is zero"),
            Self::GeometryTooLarge {
                field,
                found,
                limit,
            } => write!(f, "{field} is {found}, above the {limit} limit"),
            Self::GeometryOverflow { field } => {
                write!(f, "{field} overflows 64-bit size arithmetic")
            }
            Self::OffsetMismatch {
                region,
                recorded,
                computed,
            } => write!(
                f,
                "{region} offset is {recorded} but the geometry implies {computed}"
            ),
            Self::Misaligned {
                region,
                offset,
                align,
            } => write!(f, "{region} offset {offset} is not {align}-byte aligned"),
            Self::TotalLenMismatch { described, mapped } => write!(
                f,
                "segment describes {described} bytes but {mapped} bytes are mapped"
            ),
        }
    }
}

/// Why a receive attempt produced nothing.
///
/// Kept separate from [`ShmError`] because the two "nothing right now"
/// outcomes — an empty ring and a lagged cursor — are *normal* control flow
/// on a live route, not failures. Modelling them as `ShmError` variants would
/// make every `?` in a node's event loop wrong.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RecvError {
    /// Nothing new has been published since the consumer's cursor.
    #[error("no message available")]
    Empty,

    /// The producer overwrote messages the consumer had not read yet.
    ///
    /// Reported exactly once per gap, like a broadcast channel: the cursor is
    /// advanced to the oldest still-resident message and the next receive
    /// succeeds. Only possible under
    /// [`crate::OverflowPolicy::Overwrite`].
    #[error("lagged behind the producer by {0} messages")]
    Lagged(u64),

    /// The segment was marked closed and fully drained.
    #[error("segment closed")]
    Closed,

    /// A hard failure.
    #[error(transparent)]
    Shm(#[from] ShmError),
}

impl RecvError {
    /// Whether this outcome simply means "nothing yet; wait and try again".
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::RecvError;
    ///
    /// assert!(RecvError::Empty.is_would_block());
    /// assert!(!RecvError::Closed.is_would_block());
    /// ```
    #[must_use]
    pub const fn is_would_block(&self) -> bool {
        matches!(self, Self::Empty)
    }

    /// The number of messages missed, if this is a lag report.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::RecvError;
    ///
    /// assert_eq!(RecvError::Lagged(7).lagged(), Some(7));
    /// assert_eq!(RecvError::Empty.lagged(), None);
    /// ```
    #[must_use]
    pub const fn lagged(&self) -> Option<u64> {
        match self {
            Self::Lagged(n) => Some(*n),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn fallback_classification_matches_the_blueprint_policy() {
        assert!(ShmError::PoolExhausted { slot_count: 8 }.should_fall_back());
        assert!(
            ShmError::Closed {
                name: "x".to_owned()
            }
            .should_fall_back()
        );
        assert!(ShmError::unsupported("create").should_fall_back());
        assert!(!ShmError::Anonymous.should_fall_back());
        assert!(
            !ShmError::PayloadTooLarge {
                requested: 1,
                capacity: 0
            }
            .should_fall_back()
        );
    }

    #[test]
    fn mapping_fatal_classification() {
        assert!(
            ShmError::StaleGeneration {
                expected: 3,
                found: 2
            }
            .is_fatal_for_mapping()
        );
        assert!(
            ShmError::BadMagic {
                expected: *b"ASHM",
                found: *b"junk"
            }
            .is_fatal_for_mapping()
        );
        assert!(!ShmError::PoolExhausted { slot_count: 2 }.is_fatal_for_mapping());
    }

    #[test]
    fn corrupt_reasons_render_precisely() {
        let rendered = ShmError::CorruptHeader {
            reason: CorruptReason::OffsetMismatch {
                region: "data",
                recorded: 4096,
                computed: 8192,
            },
        }
        .to_string();
        assert!(rendered.contains("data offset is 4096"), "{rendered}");
        assert!(rendered.contains("geometry implies 8192"), "{rendered}");
    }

    #[test]
    fn every_corrupt_reason_renders_non_empty() {
        let reasons = [
            CorruptReason::MappingTooSmall {
                mapped: 4,
                required: 128,
            },
            CorruptReason::HeaderLen {
                found: 64,
                expected: 128,
            },
            CorruptReason::ZeroGeometry {
                field: "slot_count",
            },
            CorruptReason::GeometryTooLarge {
                field: "slot_count",
                found: 1 << 40,
                limit: 1 << 20,
            },
            CorruptReason::GeometryOverflow { field: "data" },
            CorruptReason::OffsetMismatch {
                region: "slots",
                recorded: 1,
                computed: 2,
            },
            CorruptReason::Misaligned {
                region: "data",
                offset: 3,
                align: 128,
            },
            CorruptReason::TotalLenMismatch {
                described: 10,
                mapped: 9,
            },
        ];
        for reason in reasons {
            assert!(!reason.to_string().is_empty());
        }
    }

    #[test]
    fn recv_error_helpers() {
        assert!(RecvError::Empty.is_would_block());
        assert_eq!(RecvError::Lagged(3).lagged(), Some(3));
        assert_eq!(RecvError::Closed.lagged(), None);
        let wrapped = RecvError::from(ShmError::Anonymous);
        assert!(matches!(wrapped, RecvError::Shm(ShmError::Anonymous)));
    }

    #[test]
    fn timeout_renders_the_budget() {
        let rendered = ShmError::Timeout {
            operation: "doorbell",
            timeout_ms: 250,
        }
        .to_string();
        assert!(rendered.contains("250ms"), "{rendered}");
        assert!(rendered.contains("doorbell"), "{rendered}");
    }

    // `Errno::NOENT` is one of the POSIX errno constants `rustix` only
    // defines on Unix; `From<rustix::io::Errno> for ShmError` itself (see
    // above) stays portable since it names no specific errno.
    #[cfg(unix)]
    #[test]
    fn errno_conversion_keeps_the_os_code() {
        let err = ShmError::from(rustix::io::Errno::NOENT);
        match err {
            ShmError::Os { source, .. } => {
                assert_eq!(
                    source.raw_os_error(),
                    Some(rustix::io::Errno::NOENT.raw_os_error())
                );
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
