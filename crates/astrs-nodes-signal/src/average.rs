//! Simple moving average ([`MovingAverage`]) and exponential moving average
//! ([`ExponentialAverage`]), and their operators
//! ([`MovingAverageOperator`], [`ExponentialAverageOperator`]).

use std::collections::{BTreeMap, VecDeque};

use astrs_operator_api::{OpError, OpEvent, OpOutput, OpResult, Operator, Status, operator};
use astrs_wire::Parameter;

use crate::error::{SignalError, SignalResult};
use crate::message::Frame;
use crate::support::decode;

/// A streaming simple moving average (SMA) over the last `capacity`
/// samples: `y[n] = (1/min(n+1, capacity)) * Σ_{i=max(0,n-capacity+1)}^{n} x[i]`.
///
/// Kept as a running sum updated by one add and (once the window is full)
/// one subtract per sample, rather than re-summing the window every push.
#[derive(Debug, Clone)]
pub struct MovingAverage {
    capacity: usize,
    window: VecDeque<f64>,
    sum: f64,
}

impl MovingAverage {
    /// Builds an empty moving average over the last `capacity` samples.
    ///
    /// # Errors
    ///
    /// [`SignalError::ZeroCapacity`] if `capacity == 0`.
    pub fn new(capacity: usize) -> SignalResult<Self> {
        if capacity == 0 {
            return Err(SignalError::ZeroCapacity);
        }
        Ok(Self {
            capacity,
            window: VecDeque::with_capacity(capacity),
            sum: 0.0,
        })
    }

    /// The window size this average was built with.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many samples are currently in the window (`<= capacity`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.window.len()
    }

    /// Whether the window has never received a sample.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.window.is_empty()
    }

    /// Clears the window, as if this average had never seen a sample.
    pub fn reset(&mut self) {
        self.window.clear();
        self.sum = 0.0;
    }

    /// Pushes one sample and returns the average over the (now possibly
    /// still filling) window.
    pub fn push(&mut self, x: f64) -> f64 {
        self.window.push_back(x);
        self.sum += x;
        if self.window.len() > self.capacity
            && let Some(oldest) = self.window.pop_front()
        {
            self.sum -= oldest;
        }
        self.sum / self.window.len() as f64
    }

    /// Pushes every sample of `input` in order, continuing this average's
    /// window across the call boundary, returning one output per input
    /// sample.
    pub fn process(&mut self, input: &[f64]) -> Vec<f64> {
        input.iter().map(|&x| self.push(x)).collect()
    }
}

/// A streaming exponential moving average (EMA):
///
/// ```text
/// y[n] = alpha*x[n] + (1 - alpha)*y[n-1],   y[-1] = 0
/// ```
///
/// `alpha` closer to `1` tracks the input faster (less smoothing); `alpha`
/// closer to `0` smooths more (tracks slower). [`ExponentialAverage::from_time_constant`]
/// derives `alpha` from a physically meaningful RC-style time constant
/// instead of an ad hoc smoothing factor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExponentialAverage {
    alpha: f64,
    state: f64,
}

impl ExponentialAverage {
    /// Builds an EMA with an explicit smoothing factor.
    ///
    /// # Errors
    ///
    /// [`SignalError::InvalidAlpha`] unless `0 < alpha <= 1`.
    pub fn new(alpha: f64) -> SignalResult<Self> {
        if !(alpha.is_finite() && alpha > 0.0 && alpha <= 1.0) {
            return Err(SignalError::InvalidAlpha { alpha });
        }
        Ok(Self { alpha, state: 0.0 })
    }

    /// Builds an EMA whose smoothing factor is derived from a time constant
    /// `tau` (seconds), the discrete-time counterpart of an analog RC
    /// lowpass's time constant: `alpha = 1 - e^{-1/(tau * Fs)}`, chosen so
    /// that after `tau` seconds a step input's response has risen to
    /// `1 - 1/e ≈ 63.2%` of its final value — exactly the same "one time
    /// constant" definition an RC circuit uses.
    ///
    /// # Errors
    ///
    /// [`SignalError::InvalidSampleRate`] if `sample_rate_hz` is not
    /// positive and finite, or [`SignalError::InvalidAlpha`] if
    /// `time_constant_s` is not positive and finite (which would otherwise
    /// produce a non-finite or out-of-range `alpha`).
    pub fn from_time_constant(sample_rate_hz: f64, time_constant_s: f64) -> SignalResult<Self> {
        if !(sample_rate_hz.is_finite() && sample_rate_hz > 0.0) {
            return Err(SignalError::InvalidSampleRate { sample_rate_hz });
        }
        if !(time_constant_s.is_finite() && time_constant_s > 0.0) {
            return Err(SignalError::InvalidAlpha {
                alpha: time_constant_s,
            });
        }
        let alpha = 1.0 - (-1.0 / (time_constant_s * sample_rate_hz)).exp();
        Self::new(alpha)
    }

    /// This average's smoothing factor.
    #[must_use]
    pub const fn alpha(&self) -> f64 {
        self.alpha
    }

    /// Resets the running average to `0.0`, as if no sample had ever
    /// arrived.
    pub fn reset(&mut self) {
        self.state = 0.0;
    }

    /// Pushes one sample and returns the updated running average.
    pub fn push(&mut self, x: f64) -> f64 {
        self.state = self.alpha.mul_add(x, (1.0 - self.alpha) * self.state);
        self.state
    }

    /// Pushes every sample of `input` in order, continuing this average's
    /// state across the call boundary, returning one output per input
    /// sample.
    pub fn process(&mut self, input: &[f64]) -> Vec<f64> {
        input.iter().map(|&x| self.push(x)).collect()
    }
}

/// Applies a simple moving average to each incoming [`Frame`].
///
/// # Configuration
///
/// ```yaml
/// operators:
///   - id: smooth
///     operator: MovingAverageOperator
///     config:
///       capacity: 8
/// ```
#[operator]
#[derive(Debug, Default)]
pub struct MovingAverageOperator {
    average: Option<MovingAverage>,
}

impl Operator for MovingAverageOperator {
    fn configure(&mut self, config: &BTreeMap<String, Parameter>) -> OpResult<()> {
        let capacity = match config.get("capacity") {
            Some(Parameter::Integer(value)) if *value > 0 => *value as usize,
            Some(other) => {
                return Err(OpError::failed(format!(
                    "\"capacity\" must be a positive integer, got {other:?}"
                )));
            }
            None => {
                return Err(OpError::failed(
                    "MovingAverageOperator requires \"capacity\"",
                ));
            }
        };
        self.average =
            Some(MovingAverage::new(capacity).map_err(|err| OpError::failed(err.to_string()))?);
        Ok(())
    }

    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input {
                metadata, payload, ..
            } => {
                let average = self
                    .average
                    .as_mut()
                    .ok_or_else(|| OpError::failed("MovingAverageOperator was never configured"))?;
                let frame: Frame = decode(payload)?;
                let averaged = average.process(&frame.to_f64());
                out.send("averaged", metadata.clone(), &Frame::from_f64(&averaged))?;
                Ok(Status::Continue)
            }
            OpEvent::Stop { .. } => Ok(Status::Finished),
            _ => Ok(Status::Continue),
        }
    }

    fn on_reload(&mut self, _out: &mut OpOutput) -> OpResult<()> {
        if let Some(average) = self.average.as_mut() {
            average.reset();
        }
        Ok(())
    }
}

/// Applies an exponential moving average to each incoming [`Frame`].
///
/// # Configuration
///
/// Either an explicit `alpha`, or `sample_rate_hz` + `time_constant_s` (see
/// [`ExponentialAverage::from_time_constant`]):
///
/// ```yaml
/// operators:
///   - id: smooth
///     operator: ExponentialAverageOperator
///     config:
///       sample_rate_hz: 100.0
///       time_constant_s: 0.5
/// ```
#[operator]
#[derive(Debug, Default)]
pub struct ExponentialAverageOperator {
    average: Option<ExponentialAverage>,
}

impl Operator for ExponentialAverageOperator {
    fn configure(&mut self, config: &BTreeMap<String, Parameter>) -> OpResult<()> {
        let average = if let Some(Parameter::Float(alpha)) = config.get("alpha") {
            ExponentialAverage::new(*alpha)
        } else {
            let sample_rate_hz = match config.get("sample_rate_hz") {
                Some(Parameter::Float(value)) => *value,
                Some(other) => {
                    return Err(OpError::failed(format!(
                        "\"sample_rate_hz\" must be a float, got {other:?}"
                    )));
                }
                None => {
                    return Err(OpError::failed(
                        "ExponentialAverageOperator requires either \"alpha\" or both \
                         \"sample_rate_hz\" and \"time_constant_s\"",
                    ));
                }
            };
            let time_constant_s = match config.get("time_constant_s") {
                Some(Parameter::Float(value)) => *value,
                Some(other) => {
                    return Err(OpError::failed(format!(
                        "\"time_constant_s\" must be a float, got {other:?}"
                    )));
                }
                None => {
                    return Err(OpError::failed(
                        "ExponentialAverageOperator requires \"time_constant_s\" alongside \
                         \"sample_rate_hz\"",
                    ));
                }
            };
            ExponentialAverage::from_time_constant(sample_rate_hz, time_constant_s)
        }
        .map_err(|err| OpError::failed(err.to_string()))?;
        self.average = Some(average);
        Ok(())
    }

    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input {
                metadata, payload, ..
            } => {
                let average = self.average.as_mut().ok_or_else(|| {
                    OpError::failed("ExponentialAverageOperator was never configured")
                })?;
                let frame: Frame = decode(payload)?;
                let averaged = average.process(&frame.to_f64());
                out.send("averaged", metadata.clone(), &Frame::from_f64(&averaged))?;
                Ok(Status::Continue)
            }
            OpEvent::Stop { .. } => Ok(Status::Finished),
            _ => Ok(Status::Continue),
        }
    }

    fn on_reload(&mut self, _out: &mut OpOutput) -> OpResult<()> {
        if let Some(average) = self.average.as_mut() {
            average.reset();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_operator_api::AstrsMessage;

    fn close(left: f64, right: f64, tol: f64) -> bool {
        (left - right).abs() < tol
    }

    // ---- MovingAverage ----

    #[test]
    fn moving_average_of_a_constant_signal_is_exactly_that_constant() {
        let mut average = MovingAverage::new(5).unwrap();
        // Even before the window fills, the average of N copies of c is c.
        for _ in 0..12 {
            assert_eq!(average.push(3.5), 3.5);
        }
    }

    #[test]
    fn moving_average_impulse_response_is_the_exact_harmonic_then_zero_sequence() {
        let mut average = MovingAverage::new(4).unwrap();
        let mut impulse = vec![0.0; 8];
        impulse[0] = 1.0;
        let response = average.process(&impulse);
        let expected = [1.0, 0.5, 1.0 / 3.0, 0.25, 0.0, 0.0, 0.0, 0.0];
        for (a, b) in response.iter().zip(expected.iter()) {
            assert!(close(*a, *b, 1e-12), "{a} vs {b}");
        }
    }

    #[test]
    fn moving_average_streaming_is_continuous_across_a_chunk_boundary() {
        let input: Vec<f64> = (0..20).map(|n| (n as f64 * 0.3).sin()).collect();
        let mut whole = MovingAverage::new(5).unwrap();
        let all_at_once = whole.process(&input);

        let mut chunked = MovingAverage::new(5).unwrap();
        let mut in_pieces = chunked.process(&input[..7]);
        in_pieces.extend(chunked.process(&input[7..]));
        assert_eq!(all_at_once, in_pieces);
    }

    #[test]
    fn moving_average_reset_clears_the_window() {
        let mut average = MovingAverage::new(3).unwrap();
        average.process(&[10.0, 20.0, 30.0]);
        assert_eq!(average.len(), 3);
        average.reset();
        assert!(average.is_empty());
        assert_eq!(average.push(7.0), 7.0);
    }

    #[test]
    fn moving_average_rejects_zero_capacity() {
        assert_eq!(
            MovingAverage::new(0).unwrap_err(),
            SignalError::ZeroCapacity
        );
    }

    // ---- ExponentialAverage ----

    /// The exact discrete step response of an EMA seeded at `0.0`:
    /// `y[n] = 1 - (1 - alpha)^{n+1}`.
    #[test]
    fn exponential_average_step_response_matches_the_closed_form_geometric_series() {
        let alpha = 0.2;
        let mut average = ExponentialAverage::new(alpha).unwrap();
        for n in 0..20u32 {
            let y = average.push(1.0);
            let expected = 1.0 - (1.0 - alpha).powi(n as i32 + 1);
            assert!(close(y, expected, 1e-9), "n={n}: {y} vs {expected}");
        }
    }

    /// The exact discrete impulse response of an EMA seeded at `0.0`:
    /// `y[n] = alpha * (1 - alpha)^n`.
    #[test]
    fn exponential_average_impulse_response_matches_the_closed_form_geometric_decay() {
        let alpha = 0.3;
        let mut average = ExponentialAverage::new(alpha).unwrap();
        let mut impulse = vec![0.0; 10];
        impulse[0] = 1.0;
        let response = average.process(&impulse);
        for (n, y) in response.iter().enumerate() {
            let expected = alpha * (1.0 - alpha).powi(n as i32);
            assert!(close(*y, expected, 1e-9), "n={n}: {y} vs {expected}");
        }
    }

    #[test]
    fn exponential_average_rejects_alpha_outside_zero_to_one() {
        assert_eq!(
            ExponentialAverage::new(0.0).unwrap_err(),
            SignalError::InvalidAlpha { alpha: 0.0 }
        );
        assert!(ExponentialAverage::new(1.5).is_err());
        assert!(ExponentialAverage::new(-0.1).is_err());
        assert!(
            ExponentialAverage::new(1.0).is_ok(),
            "alpha=1 is the boundary, inclusive"
        );
    }

    /// After exactly one time constant, a step response has risen to
    /// `1 - 1/e`, precisely by construction — see
    /// [`ExponentialAverage::from_time_constant`]'s own derivation.
    #[test]
    fn from_time_constant_reaches_one_minus_one_over_e_after_one_time_constant() {
        let sample_rate_hz = 100.0;
        let time_constant_s = 0.1; // tau * fs = 10 samples, an exact integer
        let mut average =
            ExponentialAverage::from_time_constant(sample_rate_hz, time_constant_s).unwrap();
        let samples = (time_constant_s * sample_rate_hz).round() as usize; // 10
        let mut last = 0.0;
        for _ in 0..samples {
            last = average.push(1.0);
        }
        let expected = 1.0 - std::f64::consts::E.recip();
        assert!(close(last, expected, 1e-9), "{last} vs {expected}");
    }

    #[test]
    fn from_time_constant_rejects_a_bad_sample_rate_or_time_constant() {
        assert_eq!(
            ExponentialAverage::from_time_constant(0.0, 1.0).unwrap_err(),
            SignalError::InvalidSampleRate {
                sample_rate_hz: 0.0
            }
        );
        assert!(ExponentialAverage::from_time_constant(100.0, 0.0).is_err());
        assert!(ExponentialAverage::from_time_constant(100.0, -1.0).is_err());
    }

    // ---- Operators ----

    fn frame_event(frame: &Frame) -> OpEvent {
        use astrs_time::HlcTimestamp;
        use astrs_wire::{DataId, Metadata};
        OpEvent::Input {
            id: DataId::new("in").unwrap(),
            source: "sensor/audio".parse().unwrap(),
            metadata: Metadata::new(HlcTimestamp::EPOCH),
            payload: astrs_data::ipc::encode_payload(&frame.to_record_batch().unwrap())
                .unwrap()
                .to_vec(),
        }
    }

    #[test]
    fn moving_average_operator_configure_and_run() {
        let mut op = MovingAverageOperator::default();
        let mut config = BTreeMap::new();
        config.insert("capacity".to_owned(), Parameter::Integer(4));
        op.configure(&config).unwrap();

        let mut out = OpOutput::new();
        let event = frame_event(&Frame::new(vec![2.0_f32; 4]));
        op.on_event(&event, &mut out).unwrap();
        let sends = out.drain();
        assert_eq!(sends[0].id().as_str(), "averaged");
        let decoded = astrs_data::ipc::decode_payload(sends[0].payload()).unwrap();
        let result = Frame::from_record_batch(&decoded).unwrap();
        assert_eq!(result.samples, vec![2.0, 2.0, 2.0, 2.0]);
    }

    #[test]
    fn exponential_average_operator_accepts_explicit_alpha() {
        let mut op = ExponentialAverageOperator::default();
        let mut config = BTreeMap::new();
        config.insert("alpha".to_owned(), Parameter::Float(0.5));
        op.configure(&config).unwrap();
        assert_eq!(op.average.unwrap().alpha(), 0.5);
    }

    #[test]
    fn exponential_average_operator_accepts_a_time_constant() {
        let mut op = ExponentialAverageOperator::default();
        let mut config = BTreeMap::new();
        config.insert("sample_rate_hz".to_owned(), Parameter::Float(100.0));
        config.insert("time_constant_s".to_owned(), Parameter::Float(0.1));
        op.configure(&config).unwrap();
        assert!(op.average.is_some());
    }

    #[test]
    fn exponential_average_operator_rejects_incomplete_configuration() {
        let mut op = ExponentialAverageOperator::default();
        assert!(op.configure(&BTreeMap::new()).is_err());

        let mut config = BTreeMap::new();
        config.insert("sample_rate_hz".to_owned(), Parameter::Float(100.0));
        assert!(
            op.configure(&config).is_err(),
            "sample_rate_hz without time_constant_s must be rejected"
        );
    }

    #[test]
    fn operators_reject_input_before_configuration() {
        let mut ma = MovingAverageOperator::default();
        let mut ea = ExponentialAverageOperator::default();
        let mut out = OpOutput::new();
        let event = frame_event(&Frame::new(vec![0.0_f32]));
        assert!(matches!(
            ma.on_event(&event, &mut out).unwrap_err(),
            OpError::Failed { .. }
        ));
        assert!(matches!(
            ea.on_event(&event, &mut out).unwrap_err(),
            OpError::Failed { .. }
        ));
    }

    #[test]
    fn operator_entry_names_are_snake_case() {
        assert_eq!(
            MovingAverageOperator::operator_entry().0,
            "moving_average_operator"
        );
        assert_eq!(
            ExponentialAverageOperator::operator_entry().0,
            "exponential_average_operator"
        );
    }
}
