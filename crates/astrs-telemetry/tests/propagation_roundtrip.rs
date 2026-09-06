//! Propagation round-trip tests: trace context surviving a real
//! oxicode wire encode/decode, and the publish → deliver → process
//! causality chain the blueprint (§13) promises across derived metadata.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_telemetry::propagation::{self, SpanContext};
use astrs_time::HlcTimestamp;
use astrs_wire::{Metadata, WireDecode, WireEncode};

#[test]
fn trace_context_survives_an_encode_decode_wire_hop() {
    let ctx = SpanContext::root();
    let mut outgoing = Metadata::new(HlcTimestamp::new(1, 0));
    outgoing.set_request_id("req-1");
    propagation::inject(&mut outgoing, &ctx);

    // Simulate the message actually crossing the wire: the same oxicode
    // encode/decode `astrs-wire`'s frame codec applies to every
    // `Metadata` value, not just an in-memory clone.
    let bytes = outgoing.encode_to_vec().expect("metadata encodes");
    let incoming = Metadata::decode_exact(&bytes).expect("metadata decodes");

    assert_eq!(propagation::extract(&incoming), Some(ctx));
    assert_eq!(incoming.request_id(), Some("req-1"));
}

#[test]
fn trace_context_survives_a_publish_deliver_process_chain() {
    // Hop 1 (publish): a root span is opened and injected as the first
    // message goes out.
    let root = SpanContext::root();
    let mut published = Metadata::new(HlcTimestamp::new(1, 0));
    propagation::inject(&mut published, &root);

    // Hop 2 (deliver): a subscriber derives a response/ack via
    // `follow_with_context` -- the same building block
    // `Metadata::follow()` would give a node-API response, but with the
    // trace context this crate adds preserved across the hop.
    let delivered = propagation::follow_with_context(&published);
    assert_eq!(propagation::extract(&delivered), Some(root));

    // Hop 3 (process): the receiving stage opens its own child span for
    // the work it does, and injects *that* into whatever it publishes
    // next -- still within the same trace, with a fresh span id.
    let child = root.child_span();
    let mut processed = Metadata::new(HlcTimestamp::new(2, 0));
    propagation::inject(&mut processed, &child);

    let extracted_child = propagation::extract(&processed).expect("child context present");
    assert_eq!(extracted_child.trace_id(), root.trace_id());
    assert_ne!(extracted_child.span_id(), root.span_id());
}

#[test]
fn a_peer_that_never_injects_context_produces_no_context_downstream() {
    let plain = Metadata::new(HlcTimestamp::new(1, 0));
    let derived = propagation::follow_with_context(&plain);
    assert!(propagation::extract(&derived).is_none());
}

#[test]
fn context_surviving_follow_still_encodes_and_decodes_cleanly() {
    // The combination the module docs call out explicitly: `follow()`
    // alone drops `_traceparent` on encode/decode too, since it never
    // makes it into the derived `Metadata` in the first place;
    // `follow_with_context` must survive the *same* wire round-trip.
    let root = SpanContext::root();
    let mut incoming = Metadata::new(HlcTimestamp::new(3, 0));
    propagation::inject(&mut incoming, &root);

    let plain_follow = incoming.follow();
    let plain_bytes = plain_follow.encode_to_vec().expect("encode");
    let plain_roundtripped = Metadata::decode_exact(&plain_bytes).expect("decode");
    assert!(propagation::extract(&plain_roundtripped).is_none());

    let with_context = propagation::follow_with_context(&incoming);
    let bytes = with_context.encode_to_vec().expect("encode");
    let roundtripped = Metadata::decode_exact(&bytes).expect("decode");
    assert_eq!(propagation::extract(&roundtripped), Some(root));
}
