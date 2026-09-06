//! Connection policy: timeouts, buffers, compression, mux windows, backoff.
//!
//! Every knob a transport connection has lives in one of the structs here, and
//! every one of them has a default that is correct for a robot on a local
//! network. A daemon overrides what its manifest says to override and nothing
//! else.
//!
//! ```
//! use astrs_transport::{CompressionPolicy, TransportConfig};
//! use astrs_wire::Compression;
//! use std::time::Duration;
//!
//! let config = TransportConfig::default()
//!     .with_connect_timeout(Duration::from_secs(2))
//!     .with_compression(CompressionPolicy::codec(Compression::Zstd));
//!
//! assert_eq!(config.compression.threshold_bytes, 16 * 1024);
//! ```

use std::time::Duration;

use astrs_wire::{COMPRESSION_THRESHOLD_BYTES, Compression, FeatureFlags, NegotiatedLimits, Role};

/// How long a dial may take before it is abandoned.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the greeting exchange may take (blueprint §7.2).
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a graceful close waits for the peer's acknowledgement.
pub const DEFAULT_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// The payload ceiling a connection accepts *before* the handshake widens it.
///
/// A socket that has not yet said who it is may not make this process allocate
/// more than a greeting's worth of buffer (see `astrs-wire`'s `io` docs).
pub const PRE_HANDSHAKE_MAX_PAYLOAD_BYTES: usize = 64 * 1024;
/// Frames a route may have queued for the writer before senders block.
pub const DEFAULT_ROUTE_QUEUE_DEPTH: usize = 32;
/// Frames the control channel may have queued before senders block.
pub const DEFAULT_CONTROL_QUEUE_DEPTH: usize = 256;
/// Frames a route may have waiting for its reader before backpressure bites.
pub const DEFAULT_ROUTE_WINDOW_FRAMES: u32 = 32;
/// Datagrams buffered for a receiver before the oldest is dropped.
pub const DEFAULT_DATAGRAM_QUEUE_DEPTH: usize = 64;
/// Control frames the scheduler may emit back-to-back before it must serve a
/// route, so that a control flood cannot starve the data plane.
pub const DEFAULT_CONTROL_BURST: u32 = 8;

/// The base delay of the reconnect backoff (blueprint §12).
pub const DEFAULT_BACKOFF_BASE: Duration = Duration::from_millis(250);
/// The ceiling of the reconnect backoff.
pub const DEFAULT_BACKOFF_CAP: Duration = Duration::from_secs(30);
/// Frames buffered while a connection is down before the typed overflow error.
pub const DEFAULT_RECONNECT_BUFFER_FRAMES: usize = 1_024;

/// Everything a backend needs to know to open and run a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TransportConfig {
    /// How long a dial may take.
    pub connect_timeout: Duration,
    /// How long the greeting exchange may take.
    pub handshake_timeout: Duration,
    /// How long a graceful close waits.
    pub close_timeout: Duration,
    /// Whether to disable Nagle's algorithm on TCP.
    ///
    /// Always `true` in practice: AstRS writes small control frames that must
    /// not wait for a coalescing timer. The knob exists so a bandwidth-bound
    /// deployment can measure the alternative.
    pub tcp_nodelay: bool,
    /// The read buffer each connection starts with, in bytes.
    pub read_buffer_bytes: usize,
    /// Limits this endpoint proposes at the handshake.
    pub limits: NegotiatedLimits,
    /// Capabilities this endpoint advertises.
    pub features: FeatureFlags,
    /// Compression policy for route payloads (§6.4).
    pub compression: CompressionPolicy,
    /// Per-route multiplexing policy (§6.4).
    pub mux: MuxConfig,
    /// Reconnect policy (§12).
    pub backoff: BackoffConfig,
    /// Whether the framing must carry a checksum.
    ///
    /// `None` follows the plane: mandatory on TCP and QUIC, optional on UDS
    /// (§7.1). `Some(_)` overrides both ways.
    pub require_crc: Option<bool>,
}

impl TransportConfig {
    /// The defaults, spelled out.
    #[must_use]
    pub fn new() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            close_timeout: DEFAULT_CLOSE_TIMEOUT,
            tcp_nodelay: true,
            read_buffer_bytes: 64 * 1024,
            limits: NegotiatedLimits::network(),
            features: FeatureFlags::daemon_defaults(),
            compression: CompressionPolicy::default(),
            mux: MuxConfig::default(),
            backoff: BackoffConfig::default(),
            require_crc: None,
        }
    }

    /// The policy for a node↔daemon Unix socket: no checksum, small frames,
    /// no compression (the payload is about to go into shared memory anyway).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::TransportConfig;
    /// use astrs_wire::Compression;
    ///
    /// let uds = TransportConfig::uds();
    /// assert_eq!(uds.require_crc, Some(false));
    /// assert_eq!(uds.compression.codec, Compression::None);
    /// ```
    #[must_use]
    pub fn uds() -> Self {
        Self {
            limits: NegotiatedLimits::uds(),
            compression: CompressionPolicy::disabled(),
            require_crc: Some(false),
            ..Self::new()
        }
    }

    /// Replaces the dial timeout.
    #[must_use]
    pub const fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Replaces the handshake timeout.
    #[must_use]
    pub const fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Replaces the graceful-close timeout.
    #[must_use]
    pub const fn with_close_timeout(mut self, timeout: Duration) -> Self {
        self.close_timeout = timeout;
        self
    }

    /// Replaces the proposed limits.
    #[must_use]
    pub const fn with_limits(mut self, limits: NegotiatedLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Replaces the advertised features.
    #[must_use]
    pub const fn with_features(mut self, features: FeatureFlags) -> Self {
        self.features = features;
        self
    }

    /// Replaces the compression policy.
    #[must_use]
    pub const fn with_compression(mut self, compression: CompressionPolicy) -> Self {
        self.compression = compression;
        self
    }

    /// Replaces the mux policy.
    #[must_use]
    pub const fn with_mux(mut self, mux: MuxConfig) -> Self {
        self.mux = mux;
        self
    }

    /// Replaces the reconnect policy.
    #[must_use]
    pub const fn with_backoff(mut self, backoff: BackoffConfig) -> Self {
        self.backoff = backoff;
        self
    }

    /// Forces the checksum policy instead of following the plane.
    #[must_use]
    pub const fn with_require_crc(mut self, required: Option<bool>) -> Self {
        self.require_crc = required;
        self
    }

    /// Disables Nagle coalescing (the default) or restores it.
    #[must_use]
    pub const fn with_tcp_nodelay(mut self, nodelay: bool) -> Self {
        self.tcp_nodelay = nodelay;
        self
    }

    /// The limits this endpoint proposes, with the checksum override applied.
    #[must_use]
    pub const fn proposed_limits(&self, plane_requires_crc: bool) -> NegotiatedLimits {
        let required = match self.require_crc {
            Some(explicit) => explicit,
            None => plane_requires_crc,
        };
        self.limits.with_require_crc(required)
    }

    /// The features this endpoint advertises, with codecs it will not use
    /// removed.
    ///
    /// Advertising `lz4` while the policy disables compression would let a peer
    /// send compressed frames this endpoint has no intention of producing —
    /// harmless, but a lie. The intersection keeps the greeting honest.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::{CompressionPolicy, TransportConfig};
    /// use astrs_wire::{Compression, FeatureFlags};
    ///
    /// let config = TransportConfig::new()
    ///     .with_compression(CompressionPolicy::codec(Compression::Lz4));
    /// let advertised = config.advertised_features();
    /// assert!(advertised.contains(FeatureFlags::COMPRESSION_LZ4));
    /// assert!(!advertised.contains(FeatureFlags::COMPRESSION_ZSTD));
    /// ```
    #[must_use]
    pub const fn advertised_features(&self) -> FeatureFlags {
        let codecs = FeatureFlags::COMPRESSION_LZ4.union(FeatureFlags::COMPRESSION_ZSTD);
        let wanted = FeatureFlags::for_compression(self.compression.codec);
        self.features.difference(codecs).union(wanted)
    }
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// When and how route payloads are compressed (blueprint §6.4, §7.1).
///
/// Compression is *per route* and *per payload*: the codec is whatever both
/// ends negotiated, and it only runs on payloads at or above
/// [`CompressionPolicy::threshold_bytes`], because compressing a 200-byte pose
/// costs more CPU than it saves bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CompressionPolicy {
    /// The codec to use, or [`Compression::None`] to send everything raw.
    pub codec: Compression,
    /// The smallest payload worth compressing, in bytes.
    pub threshold_bytes: usize,
    /// The largest payload worth compressing, in bytes.
    ///
    /// Above this, the latency of a single-shot compression pass dominates and
    /// the frame is sent raw. Zero means "no ceiling".
    pub ceiling_bytes: usize,
    /// Keep the compressed form only if it is at most this fraction of the
    /// original, in percent.
    ///
    /// Already-compressed payload types — JPEG frames, H.264 NAL units — expand
    /// slightly under a second pass. Measuring instead of guessing means the
    /// type-URN category (§6.4) is an optimisation, not a correctness
    /// requirement: a payload that does not shrink is simply sent raw.
    pub max_ratio_percent: u8,
    /// The zstd compression level, 1–22.
    ///
    /// This field is load-bearing, not decoration: `oxiarc_zstd`'s bare
    /// `compress` is *level 0*, which emits raw and RLE blocks only and grows
    /// a typical payload by 14 bytes. Only levels 1 and above run the LZ77
    /// matcher. Level 3 is the usual zstd default and the right trade for a
    /// 30 Hz sensor route.
    pub zstd_level: i32,
}

/// The zstd level a route uses unless the manifest says otherwise.
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;
/// The lowest zstd level that actually compresses (level 0 is raw/RLE only).
pub const MIN_ZSTD_LEVEL: i32 = 1;
/// The highest zstd level the format defines.
pub const MAX_ZSTD_LEVEL: i32 = 22;

impl CompressionPolicy {
    /// No compression at all.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            codec: Compression::None,
            threshold_bytes: COMPRESSION_THRESHOLD_BYTES as usize,
            ceiling_bytes: 0,
            max_ratio_percent: 95,
            zstd_level: DEFAULT_ZSTD_LEVEL,
        }
    }

    /// The default thresholds with a chosen codec.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::CompressionPolicy;
    /// use astrs_wire::Compression;
    ///
    /// let policy = CompressionPolicy::codec(Compression::Lz4);
    /// assert!(!policy.should_try(policy.threshold_bytes - 1));
    /// assert!(policy.should_try(policy.threshold_bytes));
    /// ```
    #[must_use]
    pub const fn codec(codec: Compression) -> Self {
        Self {
            codec,
            ..Self::disabled()
        }
    }

    /// Replaces the lower threshold.
    #[must_use]
    pub const fn with_threshold_bytes(mut self, bytes: usize) -> Self {
        self.threshold_bytes = bytes;
        self
    }

    /// Replaces the upper ceiling (zero disables it).
    #[must_use]
    pub const fn with_ceiling_bytes(mut self, bytes: usize) -> Self {
        self.ceiling_bytes = bytes;
        self
    }

    /// Replaces the keep-it ratio, in percent.
    #[must_use]
    pub const fn with_max_ratio_percent(mut self, percent: u8) -> Self {
        self.max_ratio_percent = percent;
        self
    }

    /// Replaces the zstd level, clamped into the range the format defines.
    ///
    /// Clamping rather than erroring is deliberate: a manifest that asks for
    /// level 30 wants "as small as possible", and refusing to start a robot
    /// over it would be the wrong trade. Level 0 is clamped *up* to
    /// [`MIN_ZSTD_LEVEL`], because level 0 does not compress at all and a
    /// policy that names a codec means to use it.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::{CompressionPolicy, MAX_ZSTD_LEVEL, MIN_ZSTD_LEVEL};
    /// use astrs_wire::Compression;
    ///
    /// let policy = CompressionPolicy::codec(Compression::Zstd);
    /// assert_eq!(policy.with_zstd_level(0).zstd_level, MIN_ZSTD_LEVEL);
    /// assert_eq!(policy.with_zstd_level(99).zstd_level, MAX_ZSTD_LEVEL);
    /// assert_eq!(policy.with_zstd_level(7).zstd_level, 7);
    /// ```
    #[must_use]
    pub const fn with_zstd_level(mut self, level: i32) -> Self {
        self.zstd_level = if level < MIN_ZSTD_LEVEL {
            MIN_ZSTD_LEVEL
        } else if level > MAX_ZSTD_LEVEL {
            MAX_ZSTD_LEVEL
        } else {
            level
        };
        self
    }

    /// Restricts this policy to a codec the peer also supports.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::CompressionPolicy;
    /// use astrs_wire::{Compression, FeatureFlags};
    ///
    /// let policy = CompressionPolicy::codec(Compression::Zstd);
    /// // A peer that only speaks lz4 gets no compression from a zstd policy.
    /// let agreed = policy.negotiate(FeatureFlags::COMPRESSION_LZ4);
    /// assert_eq!(agreed.codec, Compression::None);
    /// ```
    #[must_use]
    pub const fn negotiate(mut self, peer: FeatureFlags) -> Self {
        if !peer.allows_compression(self.codec) {
            self.codec = Compression::None;
        }
        self
    }

    /// This policy restricted to one specific codec.
    ///
    /// Used per route: the connection negotiated a codec at the handshake, and
    /// a route may run with a weaker one (or none) if its
    /// [`astrs_wire::RouteAcceptance`] said so.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::CompressionPolicy;
    /// use astrs_wire::Compression;
    ///
    /// let policy = CompressionPolicy::codec(Compression::Zstd);
    /// assert_eq!(policy.negotiate_codec(Compression::None).codec, Compression::None);
    /// assert_eq!(policy.negotiate_codec(Compression::Lz4).codec, Compression::Lz4);
    /// ```
    #[must_use]
    pub const fn negotiate_codec(mut self, codec: Compression) -> Self {
        self.codec = codec;
        self
    }

    /// Whether a payload of this size is a candidate for compression.
    #[must_use]
    pub const fn should_try(&self, payload_len: usize) -> bool {
        if !self.codec.is_enabled() || payload_len < self.threshold_bytes {
            return false;
        }
        self.ceiling_bytes == 0 || payload_len <= self.ceiling_bytes
    }

    /// Whether a compressed result is worth sending instead of the original.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::CompressionPolicy;
    /// use astrs_wire::Compression;
    ///
    /// let policy = CompressionPolicy::codec(Compression::Lz4);
    /// assert!(policy.is_worth_it(1_000, 400));
    /// assert!(!policy.is_worth_it(1_000, 990));
    /// ```
    #[must_use]
    pub const fn is_worth_it(&self, original_len: usize, compressed_len: usize) -> bool {
        if original_len == 0 {
            return false;
        }
        // `compressed_len * 100` cannot overflow for any frame this crate
        // accepts (the ceiling is 4 GiB, and usize is at least 64-bit on every
        // target AstRS builds for), but saturate anyway rather than wrap.
        let scaled = compressed_len.saturating_mul(100);
        scaled <= original_len.saturating_mul(self.max_ratio_percent as usize)
    }
}

impl Default for CompressionPolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

/// How a connection multiplexes routes over one peer link (blueprint §6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct MuxConfig {
    /// Whether the route mux runs at all.
    ///
    /// A CLI's one-shot request/reply connection has no routes and skips the
    /// mini-protocol entirely; a daemon↔daemon link always runs it.
    pub enabled: bool,
    /// Frames a peer may send on a fresh route before it must wait for credit.
    pub initial_window_frames: u32,
    /// Frames a route may hold in the local send queue.
    pub route_queue_depth: usize,
    /// Frames the control channel may hold in the local send queue.
    pub control_queue_depth: usize,
    /// Datagrams buffered before the oldest is dropped.
    pub datagram_queue_depth: usize,
    /// Control frames the scheduler may emit back-to-back.
    pub control_burst: u32,
    /// Grant credit back once this fraction of the window has been consumed,
    /// in percent.
    ///
    /// Granting on every frame doubles the frame rate; granting only when the
    /// window empties stalls the sender for a round trip. Half is the usual
    /// compromise.
    pub credit_grant_percent: u8,
}

impl MuxConfig {
    /// The defaults, spelled out.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            enabled: true,
            initial_window_frames: DEFAULT_ROUTE_WINDOW_FRAMES,
            route_queue_depth: DEFAULT_ROUTE_QUEUE_DEPTH,
            control_queue_depth: DEFAULT_CONTROL_QUEUE_DEPTH,
            datagram_queue_depth: DEFAULT_DATAGRAM_QUEUE_DEPTH,
            control_burst: DEFAULT_CONTROL_BURST,
            credit_grant_percent: 50,
        }
    }

    /// A configuration with no route multiplexing: control frames only.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::new()
        }
    }

    /// Replaces the initial flow-control window.
    #[must_use]
    pub const fn with_initial_window_frames(mut self, frames: u32) -> Self {
        self.initial_window_frames = frames;
        self
    }

    /// Replaces the per-route send queue depth.
    #[must_use]
    pub const fn with_route_queue_depth(mut self, depth: usize) -> Self {
        self.route_queue_depth = depth;
        self
    }

    /// Replaces the control send queue depth.
    #[must_use]
    pub const fn with_control_queue_depth(mut self, depth: usize) -> Self {
        self.control_queue_depth = depth;
        self
    }

    /// Replaces the datagram queue depth.
    #[must_use]
    pub const fn with_datagram_queue_depth(mut self, depth: usize) -> Self {
        self.datagram_queue_depth = depth;
        self
    }

    /// Replaces the control burst cap.
    #[must_use]
    pub const fn with_control_burst(mut self, frames: u32) -> Self {
        self.control_burst = frames;
        self
    }

    /// How many frames must be consumed before credit is granted back.
    ///
    /// Always at least one, so a window of one frame still makes progress.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::MuxConfig;
    ///
    /// let mux = MuxConfig::new().with_initial_window_frames(32);
    /// assert_eq!(mux.credit_grant_threshold(), 16);
    /// assert_eq!(MuxConfig::new().with_initial_window_frames(1).credit_grant_threshold(), 1);
    /// ```
    #[must_use]
    pub const fn credit_grant_threshold(&self) -> u32 {
        let scaled = (self.initial_window_frames as u64 * self.credit_grant_percent as u64) / 100;
        if scaled == 0 { 1 } else { scaled as u32 }
    }

    /// A sanity check applied before a connection starts.
    ///
    /// # Errors
    ///
    /// The message names the field that is wrong, so that a misconfigured
    /// manifest points at itself.
    pub const fn validate(&self) -> Result<(), &'static str> {
        if self.enabled {
            if self.initial_window_frames == 0 {
                return Err("mux.initial_window_frames must be at least 1");
            }
            if self.route_queue_depth == 0 {
                return Err("mux.route_queue_depth must be at least 1");
            }
            if self.control_queue_depth == 0 {
                return Err("mux.control_queue_depth must be at least 1");
            }
            if self.control_burst == 0 {
                return Err("mux.control_burst must be at least 1");
            }
            if self.credit_grant_percent == 0 || self.credit_grant_percent > 100 {
                return Err("mux.credit_grant_percent must be in 1..=100");
            }
        }
        Ok(())
    }
}

impl Default for MuxConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// The exponential backoff a [`ReconnectingConnection`](crate::ReconnectingConnection)
/// applies between dials (blueprint §12).
///
/// The schedule is `base * multiplier^attempt`, capped at `cap`, then reduced
/// by a random factor in `[1 - jitter, 1]`. Jitter is what stops a rack of
/// daemons that lost the same coordinator from redialling in lockstep forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackoffConfig {
    /// The first delay.
    pub base: Duration,
    /// The longest delay.
    pub cap: Duration,
    /// The growth factor per attempt, in percent (200 = double).
    pub multiplier_percent: u32,
    /// How much of a delay may be shaved off at random, in percent.
    pub jitter_percent: u8,
    /// Give up after this many consecutive failures; `None` retries forever.
    pub max_attempts: Option<u32>,
    /// Frames buffered while the connection is down.
    pub buffer_frames: usize,
}

impl BackoffConfig {
    /// The blueprint defaults: 250 ms base, 30 s cap, doubling, ±25% jitter.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            base: DEFAULT_BACKOFF_BASE,
            cap: DEFAULT_BACKOFF_CAP,
            multiplier_percent: 200,
            jitter_percent: 25,
            max_attempts: None,
            buffer_frames: DEFAULT_RECONNECT_BUFFER_FRAMES,
        }
    }

    /// A backoff that never waits, for tests that want determinism.
    #[must_use]
    pub const fn immediate() -> Self {
        Self {
            base: Duration::ZERO,
            cap: Duration::ZERO,
            jitter_percent: 0,
            ..Self::new()
        }
    }

    /// Replaces the base delay.
    #[must_use]
    pub const fn with_base(mut self, base: Duration) -> Self {
        self.base = base;
        self
    }

    /// Replaces the cap.
    #[must_use]
    pub const fn with_cap(mut self, cap: Duration) -> Self {
        self.cap = cap;
        self
    }

    /// Replaces the growth factor, in percent.
    #[must_use]
    pub const fn with_multiplier_percent(mut self, percent: u32) -> Self {
        self.multiplier_percent = percent;
        self
    }

    /// Replaces the jitter fraction, in percent.
    #[must_use]
    pub const fn with_jitter_percent(mut self, percent: u8) -> Self {
        self.jitter_percent = percent;
        self
    }

    /// Replaces the attempt ceiling.
    #[must_use]
    pub const fn with_max_attempts(mut self, attempts: Option<u32>) -> Self {
        self.max_attempts = attempts;
        self
    }

    /// Replaces the in-flight buffer size, in frames.
    #[must_use]
    pub const fn with_buffer_frames(mut self, frames: usize) -> Self {
        self.buffer_frames = frames;
        self
    }

    /// A sanity check applied before a reconnect loop starts.
    ///
    /// # Errors
    ///
    /// The message names the field that is wrong.
    pub const fn validate(&self) -> Result<(), &'static str> {
        if self.multiplier_percent < 100 {
            return Err("backoff.multiplier_percent must be at least 100");
        }
        if self.jitter_percent > 100 {
            return Err("backoff.jitter_percent must be at most 100");
        }
        if self.cap.as_nanos() < self.base.as_nanos() {
            return Err("backoff.cap must be at least backoff.base");
        }
        Ok(())
    }
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Which side of a connection an endpoint is, and which role it claims.
///
/// The transport needs both: the *side* decides who speaks first in the
/// handshake, the *role* is what the greeting carries (§7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Side {
    /// This end dialled: it sends `Hello` and waits for `Welcome`.
    Initiator,
    /// This end accepted: it waits for `Hello` and answers.
    Acceptor,
}

impl Side {
    /// The opposite side.
    #[must_use]
    pub const fn peer(self) -> Self {
        match self {
            Self::Initiator => Self::Acceptor,
            Self::Acceptor => Self::Initiator,
        }
    }

    /// Whether this side speaks first.
    #[must_use]
    pub const fn speaks_first(self) -> bool {
        matches!(self, Self::Initiator)
    }

    /// A stable label for logs and metrics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Initiator => "initiator",
            Self::Acceptor => "acceptor",
        }
    }

    /// The route-handle parity this side allocates from.
    ///
    /// Both ends of a connection mint route handles concurrently, so they must
    /// not collide. The initiator takes odd handles, the acceptor even ones —
    /// the same trick QUIC uses for stream ids, and the reason
    /// [`astrs_wire::RouteId::NONE`] is reserved.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::Side;
    ///
    /// assert_eq!(Side::Initiator.first_route().get(), 1);
    /// assert_eq!(Side::Acceptor.first_route().get(), 2);
    /// ```
    #[must_use]
    pub const fn first_route(self) -> astrs_wire::RouteId {
        match self {
            Self::Initiator => astrs_wire::RouteId::new(1),
            Self::Acceptor => astrs_wire::RouteId::new(2),
        }
    }

    /// Whether `route` was minted by this side.
    #[must_use]
    pub const fn owns_route(self, route: astrs_wire::RouteId) -> bool {
        if route.is_none() {
            return false;
        }
        let odd = route.get() % 2 == 1;
        match self {
            Self::Initiator => odd,
            Self::Acceptor => !odd,
        }
    }
}

/// The identity this endpoint presents in its greeting.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LocalIdentity {
    /// The role this endpoint claims (§7.2).
    pub role: Role,
    /// A human-readable label carried in `Hello`, for logs.
    pub label: Option<String>,
}

impl LocalIdentity {
    /// An identity for `role` with no label.
    #[must_use]
    pub const fn new(role: Role) -> Self {
        Self { role, label: None }
    }

    /// Attaches an operator-facing label.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

impl Default for LocalIdentity {
    fn default() -> Self {
        Self::new(Role::Peer)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::RouteId;

    #[test]
    fn the_defaults_match_the_blueprint() {
        let config = TransportConfig::new();
        assert_eq!(config.backoff.base, Duration::from_millis(250));
        assert_eq!(config.backoff.cap, Duration::from_secs(30));
        assert_eq!(config.compression.threshold_bytes, 16 * 1024);
        assert!(config.tcp_nodelay);
    }

    #[test]
    fn uds_defaults_skip_the_checksum_and_the_codec() {
        let uds = TransportConfig::uds();
        assert_eq!(uds.require_crc, Some(false));
        assert_eq!(uds.compression.codec, Compression::None);
        assert!(!uds.proposed_limits(false).require_crc);
        // …and the override wins over the plane.
        assert!(!uds.proposed_limits(true).require_crc);
    }

    #[test]
    fn the_crc_policy_follows_the_plane_when_unset() {
        let config = TransportConfig::new();
        assert!(config.proposed_limits(true).require_crc);
        assert!(!config.proposed_limits(false).require_crc);
    }

    #[test]
    fn advertised_features_never_promise_an_unused_codec() {
        let none = TransportConfig::new().with_compression(CompressionPolicy::disabled());
        let advertised = none.advertised_features();
        assert!(!advertised.contains(FeatureFlags::COMPRESSION_LZ4));
        assert!(!advertised.contains(FeatureFlags::COMPRESSION_ZSTD));

        let zstd =
            TransportConfig::new().with_compression(CompressionPolicy::codec(Compression::Zstd));
        assert!(
            zstd.advertised_features()
                .contains(FeatureFlags::COMPRESSION_ZSTD)
        );
        assert!(
            !zstd
                .advertised_features()
                .contains(FeatureFlags::COMPRESSION_LZ4)
        );
    }

    #[test]
    fn the_threshold_is_a_closed_lower_bound() {
        let policy = CompressionPolicy::codec(Compression::Lz4);
        assert!(!policy.should_try(COMPRESSION_THRESHOLD_BYTES as usize - 1));
        assert!(policy.should_try(COMPRESSION_THRESHOLD_BYTES as usize));
        assert!(policy.should_try(COMPRESSION_THRESHOLD_BYTES as usize + 1));
    }

    #[test]
    fn a_disabled_codec_never_tries() {
        let policy = CompressionPolicy::disabled();
        assert!(!policy.should_try(usize::MAX));
    }

    #[test]
    fn a_ceiling_excludes_huge_payloads() {
        let policy = CompressionPolicy::codec(Compression::Zstd)
            .with_threshold_bytes(16)
            .with_ceiling_bytes(1_024);
        assert!(policy.should_try(1_024));
        assert!(!policy.should_try(1_025));
        // Zero disables the ceiling.
        assert!(policy.with_ceiling_bytes(0).should_try(1 << 30));
    }

    #[test]
    fn a_payload_that_barely_shrinks_is_sent_raw() {
        let policy = CompressionPolicy::codec(Compression::Lz4).with_max_ratio_percent(90);
        assert!(policy.is_worth_it(1_000, 900));
        assert!(!policy.is_worth_it(1_000, 901));
        assert!(!policy.is_worth_it(0, 0));
    }

    #[test]
    fn negotiation_drops_a_codec_the_peer_lacks() {
        let zstd = CompressionPolicy::codec(Compression::Zstd);
        assert_eq!(
            zstd.negotiate(FeatureFlags::COMPRESSION_ZSTD).codec,
            Compression::Zstd
        );
        assert_eq!(
            zstd.negotiate(FeatureFlags::COMPRESSION_LZ4).codec,
            Compression::None
        );
        assert_eq!(zstd.negotiate(FeatureFlags::EMPTY).codec, Compression::None);
        // A disabled policy stays disabled whatever the peer offers.
        assert_eq!(
            CompressionPolicy::disabled()
                .negotiate(FeatureFlags::COMPRESSION_ZSTD)
                .codec,
            Compression::None
        );
    }

    #[test]
    fn mux_validation_names_the_bad_field() {
        assert!(MuxConfig::new().validate().is_ok());
        assert!(MuxConfig::disabled().validate().is_ok());
        assert_eq!(
            MuxConfig::new().with_initial_window_frames(0).validate(),
            Err("mux.initial_window_frames must be at least 1")
        );
        assert_eq!(
            MuxConfig::new().with_route_queue_depth(0).validate(),
            Err("mux.route_queue_depth must be at least 1")
        );
        assert_eq!(
            MuxConfig::new().with_control_queue_depth(0).validate(),
            Err("mux.control_queue_depth must be at least 1")
        );
        assert_eq!(
            MuxConfig::new().with_control_burst(0).validate(),
            Err("mux.control_burst must be at least 1")
        );
        let mut bad = MuxConfig::new();
        bad.credit_grant_percent = 0;
        assert!(bad.validate().is_err());
        bad.credit_grant_percent = 101;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn the_credit_threshold_is_never_zero() {
        for window in 1..=64u32 {
            let mux = MuxConfig::new().with_initial_window_frames(window);
            let threshold = mux.credit_grant_threshold();
            assert!(threshold >= 1);
            assert!(threshold <= window);
        }
    }

    #[test]
    fn backoff_validation_names_the_bad_field() {
        assert!(BackoffConfig::new().validate().is_ok());
        assert!(BackoffConfig::immediate().validate().is_ok());
        assert!(
            BackoffConfig::new()
                .with_multiplier_percent(99)
                .validate()
                .is_err()
        );
        assert!(
            BackoffConfig::new()
                .with_jitter_percent(101)
                .validate()
                .is_err()
        );
        assert!(
            BackoffConfig::new()
                .with_base(Duration::from_secs(60))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn the_two_sides_mint_disjoint_route_handles() {
        assert_eq!(Side::Initiator.peer(), Side::Acceptor);
        assert_eq!(Side::Acceptor.peer(), Side::Initiator);
        assert!(Side::Initiator.speaks_first());
        assert!(!Side::Acceptor.speaks_first());

        assert!(Side::Initiator.owns_route(RouteId::new(1)));
        assert!(!Side::Initiator.owns_route(RouteId::new(2)));
        assert!(Side::Acceptor.owns_route(RouteId::new(2)));
        assert!(!Side::Acceptor.owns_route(RouteId::new(1)));
        // The reserved handle belongs to neither.
        assert!(!Side::Initiator.owns_route(RouteId::NONE));
        assert!(!Side::Acceptor.owns_route(RouteId::NONE));

        assert_eq!(Side::Initiator.label(), "initiator");
        assert_eq!(Side::Acceptor.label(), "acceptor");
    }

    #[test]
    fn a_local_identity_carries_its_label() {
        let identity = LocalIdentity::new(Role::Daemon).with_label("daemon-a");
        assert_eq!(identity.role, Role::Daemon);
        assert_eq!(identity.label.as_deref(), Some("daemon-a"));
        assert_eq!(LocalIdentity::default().role, Role::Peer);
    }

    #[test]
    fn builders_are_chainable_and_total() {
        let config = TransportConfig::new()
            .with_connect_timeout(Duration::from_secs(1))
            .with_handshake_timeout(Duration::from_secs(2))
            .with_close_timeout(Duration::from_secs(3))
            .with_limits(NegotiatedLimits::uds())
            .with_features(FeatureFlags::TRACING)
            .with_compression(CompressionPolicy::codec(Compression::Lz4))
            .with_mux(MuxConfig::disabled())
            .with_backoff(BackoffConfig::immediate())
            .with_require_crc(Some(true))
            .with_tcp_nodelay(false);
        assert_eq!(config.connect_timeout, Duration::from_secs(1));
        assert_eq!(config.handshake_timeout, Duration::from_secs(2));
        assert_eq!(config.close_timeout, Duration::from_secs(3));
        assert!(!config.mux.enabled);
        assert_eq!(config.require_crc, Some(true));
        assert!(!config.tcp_nodelay);
        assert_eq!(config.backoff, BackoffConfig::immediate());
        assert_eq!(TransportConfig::default(), TransportConfig::new());
        assert_eq!(MuxConfig::default(), MuxConfig::new());
        assert_eq!(BackoffConfig::default(), BackoffConfig::new());
        assert_eq!(CompressionPolicy::default(), CompressionPolicy::disabled());
    }

    #[test]
    fn backoff_builders_cover_every_field() {
        let backoff = BackoffConfig::new()
            .with_base(Duration::from_millis(10))
            .with_cap(Duration::from_millis(100))
            .with_multiplier_percent(150)
            .with_jitter_percent(10)
            .with_max_attempts(Some(3))
            .with_buffer_frames(7);
        assert_eq!(backoff.base, Duration::from_millis(10));
        assert_eq!(backoff.cap, Duration::from_millis(100));
        assert_eq!(backoff.multiplier_percent, 150);
        assert_eq!(backoff.jitter_percent, 10);
        assert_eq!(backoff.max_attempts, Some(3));
        assert_eq!(backoff.buffer_frames, 7);
        assert!(backoff.validate().is_ok());
    }

    #[test]
    fn mux_builders_cover_every_field() {
        let mux = MuxConfig::new()
            .with_initial_window_frames(8)
            .with_route_queue_depth(9)
            .with_control_queue_depth(10)
            .with_datagram_queue_depth(11)
            .with_control_burst(12);
        assert_eq!(mux.initial_window_frames, 8);
        assert_eq!(mux.route_queue_depth, 9);
        assert_eq!(mux.control_queue_depth, 10);
        assert_eq!(mux.datagram_queue_depth, 11);
        assert_eq!(mux.control_burst, 12);
    }
}
