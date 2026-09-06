//! Span-context propagation through [`astrs_wire::Metadata`].
//!
//! Blueprint §13: "span context propagates in message metadata; per-message
//! causality (publish → deliver → process) is reconstructible from a
//! recording." `astrs-wire` owns `Metadata` itself and its well-known keys
//! (`request_id`, `goal_id`, ...) but deliberately does not know about W3C
//! trace context — that vocabulary belongs to this crate, per the task
//! split: "check astrs-wire's key constants, add usage helpers here not
//! there." This module is those helpers.
//!
//! # The key
//!
//! [`TRACEPARENT_KEY`] (`"_traceparent"`) is `_`-prefixed, so it follows
//! `astrs-wire`'s existing internal-key convention exactly: it rides
//! alongside a message like `_schema_hash` does, and
//! [`astrs_wire::Metadata::strip_internal`] removes it before user code
//! ever sees it. [`TRACESTATE_KEY`] is reserved for a future W3C
//! `tracestate` value; nothing in this crate writes it yet.
//!
//! # `Metadata::follow()` does not carry trace context — use
//! [`follow_with_context`] instead
//!
//! [`astrs_wire::Metadata::follow`] is the node-API's building block for
//! "derive the outgoing metadata for a response to this input", and by
//! design it keeps only the correlation and stream keys, stripping every
//! `_`-prefixed internal one — including `_traceparent`. That is correct
//! for `follow` itself (it must not leak *arbitrary* internal plumbing),
//! but it means a plain `incoming.follow()` silently breaks the
//! publish→deliver→process causality chain the blueprint promises at
//! exactly the hop where a node turns an input into an output.
//! [`follow_with_context`] is `follow()` plus re-injecting whatever trace
//! context `incoming` carried, and is what node-shaped code should call
//! instead whenever a span is in play.
//!
//! # Examples
//!
//! ```
//! use astrs_telemetry::propagation::{self, SpanContext};
//! use astrs_wire::Metadata;
//! use astrs_time::HlcTimestamp;
//!
//! let root = SpanContext::root();
//! let mut outgoing = Metadata::new(HlcTimestamp::new(1, 0));
//! propagation::inject(&mut outgoing, &root);
//!
//! // ... the message crosses the wire ...
//!
//! let incoming = outgoing;
//! let extracted = propagation::extract(&incoming).expect("context round-trips");
//! assert_eq!(extracted.trace_id(), root.trace_id());
//! assert_eq!(extracted.span_id(), root.span_id());
//! ```

use std::sync::OnceLock;

use astrs_wire::{Metadata, ParamKey, Parameter};

use crate::ids::{decode_hex_exact, encode_hex, new_span_id, new_trace_id};

/// The internal metadata key carrying the W3C `traceparent` value.
pub const TRACEPARENT_KEY: &str = "_traceparent";

/// The internal metadata key reserved for a future W3C `tracestate`
/// value. Not written by this crate yet; reserved so a later addition
/// does not need a second pass over every call site that reads
/// `TRACEPARENT_KEY`.
pub const TRACESTATE_KEY: &str = "_tracestate";

/// The only `traceparent` format version this crate writes or accepts.
const TRACEPARENT_VERSION: u8 = 0x00;

/// `traceparent` version `0xff` is reserved by the W3C spec and must be
/// treated as invalid.
const TRACEPARENT_VERSION_INVALID: u8 = 0xff;

/// Bit 0 of the `traceparent` flags byte: the sampled flag.
const SAMPLED_FLAG: u8 = 0x01;

/// A W3C Trace Context span identity: a 128-bit trace id, a 64-bit span
/// id, and a sampled flag.
///
/// # Examples
///
/// ```
/// use astrs_telemetry::propagation::SpanContext;
///
/// let root = SpanContext::root();
/// assert!(root.sampled());
/// assert_eq!(root.trace_id_hex().len(), 32);
/// assert_eq!(root.span_id_hex().len(), 16);
///
/// let child = root.child_span();
/// assert_eq!(child.trace_id(), root.trace_id(), "children share the trace");
/// assert_ne!(child.span_id(), root.span_id(), "but not the span");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanContext {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    sampled: bool,
}

impl SpanContext {
    /// Builds a context from explicit ids, sampled by default.
    #[must_use]
    pub const fn new(trace_id: [u8; 16], span_id: [u8; 8]) -> Self {
        Self {
            trace_id,
            span_id,
            sampled: true,
        }
    }

    /// Starts a brand-new trace: a fresh trace id and a fresh root span
    /// id, sampled by default.
    #[must_use]
    pub fn root() -> Self {
        Self::new(new_trace_id(), new_span_id())
    }

    /// Derives a child span within the same trace: same trace id, a
    /// fresh span id, and the same sampling decision (W3C Trace Context
    /// says the sampled flag is a recommendation that propagates
    /// downstream unchanged unless a participant deliberately
    /// overrides it — see [`SpanContext::with_sampled`]).
    #[must_use]
    pub fn child_span(&self) -> Self {
        Self {
            trace_id: self.trace_id,
            span_id: new_span_id(),
            sampled: self.sampled,
        }
    }

    /// Overrides the sampled flag, builder-style.
    #[must_use]
    pub const fn with_sampled(mut self, sampled: bool) -> Self {
        self.sampled = sampled;
        self
    }

    /// The 128-bit trace id.
    #[must_use]
    pub const fn trace_id(&self) -> [u8; 16] {
        self.trace_id
    }

    /// The 64-bit span id.
    #[must_use]
    pub const fn span_id(&self) -> [u8; 8] {
        self.span_id
    }

    /// Whether this span is marked sampled.
    #[must_use]
    pub const fn sampled(&self) -> bool {
        self.sampled
    }

    /// The trace id as 32 lowercase hex characters — OTLP's `traceId`
    /// JSON encoding (blueprint §13).
    #[must_use]
    pub fn trace_id_hex(&self) -> String {
        encode_hex(&self.trace_id)
    }

    /// The span id as 16 lowercase hex characters — OTLP's `spanId` JSON
    /// encoding.
    #[must_use]
    pub fn span_id_hex(&self) -> String {
        encode_hex(&self.span_id)
    }

    /// Renders the W3C `traceparent` header value:
    /// `"00-<32 hex>-<16 hex>-<01 or 00>"`.
    #[must_use]
    pub fn to_traceparent(&self) -> String {
        format!(
            "{TRACEPARENT_VERSION:02x}-{}-{}-{:02x}",
            self.trace_id_hex(),
            self.span_id_hex(),
            if self.sampled { SAMPLED_FLAG } else { 0 },
        )
    }

    /// Parses a W3C `traceparent` header value.
    ///
    /// Returns `None` for anything malformed — the wrong number of
    /// `-`-separated fields, non-hex characters, the reserved `ff`
    /// version, or an all-zero trace/span id (invalid per the spec) —
    /// rather than erroring loudly: a `traceparent` comes from a peer
    /// this process does not control, and a malformed one should simply
    /// mean "no trace context available," not a decode failure that
    /// derails message delivery.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_telemetry::propagation::SpanContext;
    ///
    /// // The example from the W3C Trace Context specification.
    /// let ctx = SpanContext::parse_traceparent(
    ///     "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
    /// )
    /// .expect("valid traceparent");
    /// assert_eq!(ctx.trace_id_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
    /// assert_eq!(ctx.span_id_hex(), "00f067aa0ba902b7");
    /// assert!(ctx.sampled());
    ///
    /// assert!(SpanContext::parse_traceparent("garbage").is_none());
    /// ```
    #[must_use]
    pub fn parse_traceparent(value: &str) -> Option<Self> {
        let mut fields = value.split('-');
        let version_hex = fields.next()?;
        let trace_id_hex = fields.next()?;
        let span_id_hex = fields.next()?;
        let flags_hex = fields.next()?;
        // Version 00 is a closed, exactly-4-field format; this crate
        // does not need to parse the forward-compatible extension
        // fields a future version might append, so any extra field is
        // simply treated as malformed.
        if fields.next().is_some() {
            return None;
        }

        let [version] = decode_hex_exact::<1>(version_hex)?;
        if version == TRACEPARENT_VERSION_INVALID {
            return None;
        }
        let trace_id = decode_hex_exact::<16>(trace_id_hex)?;
        let span_id = decode_hex_exact::<8>(span_id_hex)?;
        let [flags] = decode_hex_exact::<1>(flags_hex)?;
        if trace_id == [0u8; 16] || span_id == [0u8; 8] {
            return None;
        }
        Some(Self {
            trace_id,
            span_id,
            sampled: flags & SAMPLED_FLAG != 0,
        })
    }
}

/// The [`ParamKey`] for [`TRACEPARENT_KEY`], parsed once and cached.
///
/// `TRACEPARENT_KEY` is a fixed, short, `[A-Za-z0-9_.-]+` string that
/// satisfies `ParamKey`'s grammar by construction — verified by this
/// module's own `well_known_keys_are_valid_param_keys` test, mirroring
/// the identical invariant `astrs-wire` checks for its own well-known
/// keys. Caching it matters here specifically because, unlike a metric
/// registration, span-context propagation runs on a per-message path
/// (every hop injects or follows a context); re-validating the same
/// 13-byte constant on every call would be pure waste. Routing through
/// `Option` rather than unwrapping keeps [`inject`]/[`follow_with_context`]
/// infallible without a panic on a path that is unreachable in practice.
fn traceparent_key() -> Option<ParamKey> {
    static KEY: OnceLock<Option<ParamKey>> = OnceLock::new();
    KEY.get_or_init(|| ParamKey::new(TRACEPARENT_KEY).ok())
        .clone()
}

/// Injects `ctx` into `metadata` as a `traceparent` value.
///
/// Infallible: see the crate-private `traceparent_key` for why the one way this could
/// fail in principle is unreachable for this module's own constant.
///
/// # Examples
///
/// ```
/// use astrs_telemetry::propagation::{self, SpanContext};
/// use astrs_wire::Metadata;
/// use astrs_time::HlcTimestamp;
///
/// let mut metadata = Metadata::new(HlcTimestamp::EPOCH);
/// propagation::inject(&mut metadata, &SpanContext::root());
/// assert!(metadata.contains_key(propagation::TRACEPARENT_KEY));
/// ```
pub fn inject(metadata: &mut Metadata, ctx: &SpanContext) {
    if let Some(key) = traceparent_key() {
        metadata.insert_key(key, Parameter::String(ctx.to_traceparent()));
    }
}

/// Extracts a [`SpanContext`] from `metadata`'s `traceparent` value, if
/// present and well-formed.
///
/// # Examples
///
/// ```
/// use astrs_telemetry::propagation::{self, SpanContext};
/// use astrs_wire::Metadata;
/// use astrs_time::HlcTimestamp;
///
/// let metadata = Metadata::new(HlcTimestamp::EPOCH);
/// assert!(propagation::extract(&metadata).is_none(), "nothing injected yet");
/// ```
#[must_use]
pub fn extract(metadata: &Metadata) -> Option<SpanContext> {
    let raw = metadata.get(TRACEPARENT_KEY)?.as_str()?;
    SpanContext::parse_traceparent(raw)
}

/// [`astrs_wire::Metadata::follow`], plus re-injecting `incoming`'s trace
/// context — see the module docs for why plain `follow()` alone loses
/// it.
///
/// A no-op with respect to trace context when `incoming` did not carry
/// one: the result is then identical to `incoming.follow()`.
///
/// # Examples
///
/// ```
/// use astrs_telemetry::propagation::{self, SpanContext};
/// use astrs_wire::{Metadata, Parameter};
/// use astrs_time::HlcTimestamp;
///
/// let mut incoming = Metadata::new(HlcTimestamp::new(5, 0))
///     .with("request_id", Parameter::String("r-1".into()))
///     .expect("valid key");
/// let root = SpanContext::root();
/// propagation::inject(&mut incoming, &root);
///
/// // Plain `follow()` drops the trace context ...
/// assert!(propagation::extract(&incoming.follow()).is_none());
/// // ... `follow_with_context` does not.
/// let outgoing = propagation::follow_with_context(&incoming);
/// assert_eq!(outgoing.request_id(), Some("r-1"), "follow()'s own job still happens");
/// assert_eq!(propagation::extract(&outgoing), Some(root));
/// ```
#[must_use]
pub fn follow_with_context(incoming: &Metadata) -> Metadata {
    let mut outgoing = incoming.follow();
    if let Some(raw) = incoming.get(TRACEPARENT_KEY).and_then(Parameter::as_str)
        && let Some(key) = traceparent_key()
    {
        outgoing.insert_key(key, Parameter::String(raw.to_owned()));
    }
    outgoing
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn well_known_keys_are_valid_param_keys() {
        assert!(ParamKey::new(TRACEPARENT_KEY).is_ok());
        assert!(ParamKey::new(TRACESTATE_KEY).is_ok());
    }

    #[test]
    fn traceparent_round_trips_through_display_and_parse() {
        let ctx = SpanContext::root();
        let rendered = ctx.to_traceparent();
        let parsed = SpanContext::parse_traceparent(&rendered).unwrap();
        assert_eq!(parsed, ctx);
    }

    #[test]
    fn unsampled_flag_round_trips() {
        let ctx = SpanContext::root().with_sampled(false);
        let rendered = ctx.to_traceparent();
        assert!(rendered.ends_with("-00"));
        let parsed = SpanContext::parse_traceparent(&rendered).unwrap();
        assert!(!parsed.sampled());
    }

    #[test]
    fn parses_the_w3c_spec_example() {
        let ctx = SpanContext::parse_traceparent(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        )
        .unwrap();
        assert_eq!(ctx.trace_id_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(ctx.span_id_hex(), "00f067aa0ba902b7");
        assert!(ctx.sampled());
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(SpanContext::parse_traceparent("").is_none());
        assert!(SpanContext::parse_traceparent("garbage").is_none());
        assert!(SpanContext::parse_traceparent("00-short-00f067aa0ba902b7-01").is_none());
        assert!(
            SpanContext::parse_traceparent(
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra"
            )
            .is_none()
        );
        assert!(
            SpanContext::parse_traceparent(
                "zz-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
            )
            .is_none()
        );
    }

    #[test]
    fn rejects_the_reserved_ff_version() {
        assert!(
            SpanContext::parse_traceparent(
                "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
            )
            .is_none()
        );
    }

    #[test]
    fn rejects_all_zero_trace_or_span_id() {
        assert!(
            SpanContext::parse_traceparent(
                "00-00000000000000000000000000000000-00f067aa0ba902b7-01"
            )
            .is_none()
        );
        assert!(
            SpanContext::parse_traceparent(
                "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01"
            )
            .is_none()
        );
    }

    #[test]
    fn child_span_keeps_the_trace_and_sampling_but_not_the_span_id() {
        let root = SpanContext::root().with_sampled(false);
        let child = root.child_span();
        assert_eq!(child.trace_id(), root.trace_id());
        assert_ne!(child.span_id(), root.span_id());
        assert_eq!(child.sampled(), root.sampled());
    }

    #[test]
    fn inject_then_extract_round_trips() {
        let mut metadata = Metadata::new(astrs_time::HlcTimestamp::EPOCH);
        let ctx = SpanContext::root();
        inject(&mut metadata, &ctx);
        assert_eq!(extract(&metadata), Some(ctx));
    }

    #[test]
    fn extract_without_injection_is_none() {
        let metadata = Metadata::new(astrs_time::HlcTimestamp::EPOCH);
        assert!(extract(&metadata).is_none());
    }

    #[test]
    fn extract_ignores_a_malformed_traceparent_value() {
        let mut metadata = Metadata::new(astrs_time::HlcTimestamp::EPOCH);
        metadata
            .insert(
                TRACEPARENT_KEY,
                Parameter::String("not-a-traceparent".into()),
            )
            .unwrap();
        assert!(extract(&metadata).is_none());
    }

    #[test]
    fn plain_follow_drops_trace_context_but_follow_with_context_keeps_it() {
        let mut incoming = Metadata::new(astrs_time::HlcTimestamp::new(9, 0));
        let ctx = SpanContext::root();
        inject(&mut incoming, &ctx);

        let plain = incoming.follow();
        assert!(
            extract(&plain).is_none(),
            "documents astrs-wire's own follow() semantics"
        );

        let with_context = follow_with_context(&incoming);
        assert_eq!(extract(&with_context), Some(ctx));
    }

    #[test]
    fn follow_with_context_is_a_plain_follow_when_no_context_was_present() {
        let mut incoming = Metadata::new(astrs_time::HlcTimestamp::new(1, 0));
        incoming.set_seq(3);
        let expected = incoming.follow();
        let actual = follow_with_context(&incoming);
        assert_eq!(actual, expected);
    }

    #[test]
    fn strip_internal_removes_the_traceparent_key() {
        let mut metadata = Metadata::new(astrs_time::HlcTimestamp::EPOCH);
        inject(&mut metadata, &SpanContext::root());
        assert_eq!(metadata.strip_internal(), 1);
        assert!(extract(&metadata).is_none());
    }
}
