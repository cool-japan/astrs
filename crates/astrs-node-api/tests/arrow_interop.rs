//! `send_arrow` end to end (blueprint §9.1's `send_arrow(ArrayRef)
//! [arrow-interop]`), behind this crate's `arrow-interop` feature.
//!
//! The round trip is the test: an AstRS record batch becomes an arrow-rs one
//! through `astrs-data`'s bridge, that arrow value goes out through
//! [`RawOutput::send_arrow`](astrs_node_api::RawOutput::send_arrow), and the
//! consumer decodes AstRS columns that equal what the producer started with.
//! Converting a batch to arrow and back in memory would prove only that the
//! bridge is self-consistent; sending it proves the wire path accepts what the
//! bridge produces, which is the part `send_arrow` actually adds.
//!
//! # Why no arrow-rs type is named here
//!
//! The same reason `src/output/arrow.rs` gives for the signature: this crate
//! is not a direct parent of arrow-rs and must not become one (the
//! workspace's `deny.toml` confines every arrow-rs edge to `astrs-data`), and
//! a test is a target of this crate. So the arrow value is obtained from
//! `astrs_data::interop::to_arrow_record_batch` and only ever held in an
//! inferred binding — which also happens to be exactly how a real
//! arrow-native pipeline hands one over.

#![cfg(feature = "arrow-interop")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use astrs_data::array::{Float64Array, Int32Array, IntoArrayRef, StringArray};
use astrs_data::interop::to_arrow_record_batch;
use astrs_data::{ArrayRef, RecordBatch};
use astrs_node_api::output::ArrowSendError;
use astrs_node_api::testing::TestHarness;

/// The deadline every wait in this file uses.
const WAIT: Duration = Duration::from_secs(5);

/// Publishes `batch` as an arrow-rs record batch and returns what the
/// consumer decoded.
fn round_trip_through_arrow(batch: &RecordBatch) -> RecordBatch {
    let (mut producer, mut consumer) =
        TestHarness::pair("arrow-sender", "port", "arrow-receiver", "port").unwrap();
    let mut output = producer.node.raw_output("port").unwrap();

    // The one arrow-rs value in the file, and never named: `to_arrow_record_batch`
    // returns `arrow_array::RecordBatch`, and `send_arrow`'s bound is what
    // accepts it.
    let arrow_batch = to_arrow_record_batch(batch).expect("the batch maps onto arrow-rs");
    let metadata = producer.node.metadata();
    output
        .send_arrow(&arrow_batch, metadata)
        .expect("the arrow batch publishes");

    let event = consumer
        .events
        .recv_timeout(WAIT)
        .unwrap()
        .expect("the payload arrives");
    let (_, _, payload) = event.into_input().expect("an input");
    payload.batch().expect("a decodable payload").clone()
}

/// Asserts two batches carry the same schema and the same column values.
fn assert_same(left: &RecordBatch, right: &RecordBatch) {
    assert_eq!(left.num_rows(), right.num_rows(), "row count");
    assert_eq!(left.num_columns(), right.num_columns(), "column count");
    assert_eq!(left.schema(), right.schema(), "schema");
    for (index, (a, b)) in left.columns().iter().zip(right.columns()).enumerate() {
        assert_eq!(a.as_ref(), b.as_ref(), "column {index}");
    }
}

#[test]
fn a_numeric_batch_survives_the_arrow_bridge_and_the_wire() {
    let batch = RecordBatch::from_payload(
        Float64Array::from_values([1.5, -2.5, 0.0, 1e9]).into_array_ref(),
    );
    assert_same(&batch, &round_trip_through_arrow(&batch));
}

#[test]
fn a_batch_with_nulls_keeps_its_validity_across_the_bridge() {
    let batch = RecordBatch::from_payload(
        Int32Array::from_opt_iter([Some(1), None, Some(3), None]).into_array_ref(),
    );
    let back = round_trip_through_arrow(&batch);
    assert_same(&batch, &back);
    let column = back.payload_column().expect("the payload column");
    assert_eq!(column.null_count(), 2, "both nulls survive");
}

#[test]
fn a_string_batch_survives_the_arrow_bridge_and_the_wire() {
    let batch = RecordBatch::from_payload(
        StringArray::from_values(["frames", "detections", ""]).into_array_ref(),
    );
    assert_same(&batch, &round_trip_through_arrow(&batch));
}

#[test]
fn a_typed_output_offers_the_same_arrow_send() {
    // `Output<T>` carries `send_arrow` for the same reason it carries
    // `send_batch`: a typed handle should not be a reason to go and fetch the
    // raw one.
    let mut harness = astrs_node_api::Node::init_testing().expect("the harness starts");
    let mut typed = harness
        .node
        .output::<astrs_node_api::message::Scalar<f64>>(TestHarness::DEFAULT_OUTPUT)
        .expect("a typed handle");

    let batch = RecordBatch::from_payload(Float64Array::from_values([7.0]).into_array_ref());
    let arrow_batch = to_arrow_record_batch(&batch).expect("maps onto arrow-rs");
    let metadata = harness.node.metadata();
    typed
        .send_arrow(&arrow_batch, metadata)
        .expect("the arrow batch publishes through the typed skin");

    let id = astrs_wire::DataId::new(TestHarness::DEFAULT_OUTPUT).expect("a valid id");
    let sends = harness
        .daemon
        .wait_for_sends(harness.node.id(), &id, 1, WAIT)
        .expect("the daemon observes the send");
    assert!(sends[0].bytes().is_some_and(|bytes| !bytes.is_empty()));
    harness.shutdown();
}

#[test]
fn a_send_on_a_closed_output_reports_the_send_half_of_the_error() {
    // The other arm of `ArrowSendError`: the conversion succeeded, the send
    // did not. Keeping both arms exercised is what stops the enum from
    // quietly collapsing into one.
    let mut harness = astrs_node_api::Node::init_testing().expect("the harness starts");
    let mut output = harness
        .node
        .raw_output(TestHarness::DEFAULT_OUTPUT)
        .expect("the harness declares this output");
    output.close().expect("closing succeeds once");

    let batch = RecordBatch::from_payload(Float64Array::from_values([1.0]).into_array_ref());
    let arrow_batch = to_arrow_record_batch(&batch).expect("maps onto arrow-rs");
    let metadata = harness.node.metadata();
    let error = output
        .send_arrow(&arrow_batch, metadata)
        .expect_err("a closed output refuses");
    assert!(
        matches!(error, ArrowSendError::Send(_)),
        "a closed output is a send failure, not a conversion failure: {error:?}"
    );
    // `#[error(transparent)]` means the rendered message is the underlying
    // error's own, not a wrapper's.
    assert!(!error.to_string().is_empty());
    harness.shutdown();
}

#[test]
fn send_array_publishes_a_single_column_without_the_bridge() {
    // The unfeatured shorthand behind §9.1's `send_arrow(ArrayRef)` — the
    // landing point for an `arrow_array::ArrayRef` that has already been
    // through `astrs_data::interop::from_arrow_array`.
    let (mut producer, mut consumer) =
        TestHarness::pair("array-sender", "port", "array-receiver", "port").unwrap();
    let mut output = producer.node.raw_output("port").unwrap();

    let column: ArrayRef = Float64Array::from_values([3.25, 4.5]).into_array_ref();
    let metadata = producer.node.metadata();
    output
        .send_array(ArrayRef::clone(&column), metadata)
        .expect("the column publishes");

    let event = consumer
        .events
        .recv_timeout(WAIT)
        .unwrap()
        .expect("the payload arrives");
    let (_, _, payload) = event.into_input().expect("an input");
    let batch = payload.batch().expect("a decodable payload");
    assert_eq!(
        batch
            .payload_column()
            .expect("the single payload column")
            .as_ref(),
        column.as_ref()
    );
}
