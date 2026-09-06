//! [`Frame`] — the common wire shape for a block of real-valued samples.
//!
//! Windowing, biquad and FIR filtering, decimation, interpolation, and both
//! averages all read and write this one shape; only the frequency-domain
//! stages (`crate::fft`, `crate::spectrogram`) need a richer message of
//! their own.

use astrs_data::{RecordBatch, Result as DataResult};
use astrs_node_api::message::FromPayload;
use astrs_operator_api::AstrsMessage;

/// A block of real-valued samples, oldest first.
///
/// ```
/// use astrs_nodes_signal::Frame;
/// use astrs_operator_api::AstrsMessage;
///
/// let frame = Frame::new(vec![1.0_f32, 2.0, 3.0]);
/// let batch = frame.to_record_batch()?;
/// assert_eq!(Frame::from_record_batch(&batch)?, frame);
/// # Ok::<(), astrs_data::DataError>(())
/// ```
#[derive(Debug, Clone, PartialEq, AstrsMessage)]
#[astrs(urn = "std/signal/v1/Frame")]
pub struct Frame {
    /// The samples, in time order.
    pub samples: Vec<f32>,
}

impl Frame {
    /// Builds a frame from any `Vec<f32>`-convertible source.
    #[must_use]
    pub fn new(samples: impl Into<Vec<f32>>) -> Self {
        Self {
            samples: samples.into(),
        }
    }

    /// This frame's samples, widened to `f64` — the precision every design
    /// and filtering routine in this crate computes in internally.
    #[must_use]
    pub fn to_f64(&self) -> Vec<f64> {
        self.samples
            .iter()
            .map(|&sample| f64::from(sample))
            .collect()
    }

    /// Builds a frame from `f64` samples, narrowing each one to `f32` for
    /// the wire.
    #[must_use]
    pub fn from_f64(samples: &[f64]) -> Self {
        Self {
            samples: samples.iter().map(|&sample| sample as f32).collect(),
        }
    }
}

// The read-only bridge into `astrs-node-api`'s standalone-node message
// story (`Payload::view::<Frame>()`), matching the pattern
// `examples/rust-pipeline` uses for its own `Detections` type: a plain
// three-line forward to the `AstrsMessage` decoder this derive already
// generated.
impl FromPayload for Frame {
    fn from_batch(batch: &RecordBatch) -> DataResult<Self> {
        <Self as AstrsMessage>::from_record_batch(batch)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn new_accepts_a_vec_or_an_array() {
        assert_eq!(Frame::new(vec![1.0_f32, 2.0]).samples, vec![1.0, 2.0]);
        assert_eq!(Frame::new([1.0_f32, 2.0]).samples, vec![1.0, 2.0]);
    }

    #[test]
    fn to_f64_and_from_f64_round_trip_within_f32_precision() {
        let frame = Frame::new(vec![1.5_f32, -2.25, 0.0]);
        let widened = frame.to_f64();
        assert_eq!(widened, vec![1.5, -2.25, 0.0]);
        assert_eq!(Frame::from_f64(&widened), frame);
    }

    #[test]
    fn round_trips_through_a_record_batch() {
        let frame = Frame::new(vec![1.0_f32, 2.0, 3.0]);
        let batch = frame.to_record_batch().unwrap();
        assert_eq!(Frame::from_record_batch(&batch).unwrap(), frame);
        assert_eq!(<Frame as FromPayload>::from_batch(&batch).unwrap(), frame);
    }

    #[test]
    fn an_empty_frame_round_trips_too() {
        let frame = Frame::new(Vec::<f32>::new());
        let batch = frame.to_record_batch().unwrap();
        assert_eq!(Frame::from_record_batch(&batch).unwrap(), frame);
    }
}
