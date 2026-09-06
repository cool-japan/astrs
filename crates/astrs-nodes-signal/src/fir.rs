//! Windowed-sinc FIR filter design, [`FirFilter`] (a streaming direct-form
//! convolution engine), and [`FirOperator`].
//!
//! # Windowed-sinc design
//!
//! The ideal brick-wall lowpass filter's impulse response is an infinite,
//! non-causal sinc; [`design_lowpass`] truncates it to `num_taps` samples
//! centered on `(num_taps - 1) / 2` and multiplies by a
//! [`WindowFunction`]'s **symmetric** taper (see `crate::window` for why
//! FIR design wants the symmetric, not periodic, form) to tame the
//! truncation's Gibbs ringing (reference: Smith, *The Scientist and
//! Engineer's Guide to DSP*, ch. 16; Oppenheim & Schafer, *Discrete-Time
//! Signal Processing*, §7.5):
//!
//! ```text
//! h_ideal[n] = 2*fc * sinc(2*fc*(n - M/2)),   M = num_taps - 1,  fc = cutoff/sample_rate
//! h[n]       = h_ideal[n] * window_symmetric(n, num_taps)
//! ```
//!
//! normalized so `Σh[n] = 1` (exactly unity gain at DC). [`design_highpass`]
//! and [`design_bandpass`]/[`design_bandstop`] build on that lowpass
//! prototype through the two standard combination tricks (spectral inversion
//! and lowpass differencing) documented on each function.

use std::collections::{BTreeMap, VecDeque};
use std::f64::consts::PI;

use astrs_operator_api::{OpError, OpEvent, OpOutput, OpResult, Operator, Status, operator};
use astrs_wire::Parameter;

use crate::error::{SignalError, SignalResult};
use crate::message::Frame;
use crate::support::decode;
use crate::window::WindowFunction;

/// The normalized sinc function `sin(pi*x) / (pi*x)`, with the removable
/// singularity at `x == 0` filled in by its limit, `1.0`.
fn normalized_sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    }
}

/// Checks the arguments every `design_*` function shares: at least one tap,
/// a positive finite sample rate, and a cutoff strictly between DC and
/// Nyquist.
fn validate_lowpass_inputs(
    num_taps: usize,
    cutoff_hz: f64,
    sample_rate_hz: f64,
) -> SignalResult<f64> {
    if num_taps == 0 {
        return Err(SignalError::NoTaps { num_taps });
    }
    if !(sample_rate_hz.is_finite() && sample_rate_hz > 0.0) {
        return Err(SignalError::InvalidSampleRate { sample_rate_hz });
    }
    let nyquist_hz = sample_rate_hz / 2.0;
    if !(cutoff_hz.is_finite() && cutoff_hz > 0.0 && cutoff_hz < nyquist_hz) {
        return Err(SignalError::CutoffOutOfRange {
            cutoff_hz,
            nyquist_hz,
        });
    }
    Ok(nyquist_hz)
}

/// Designs a windowed-sinc lowpass FIR filter with `num_taps` taps, unity
/// gain at DC. See the module documentation for the reference
/// formula.
///
/// # Errors
///
/// [`SignalError::NoTaps`] if `num_taps == 0`,
/// [`SignalError::InvalidSampleRate`] if `sample_rate_hz` is not positive
/// and finite, or [`SignalError::CutoffOutOfRange`] if `cutoff_hz` is not
/// strictly between DC and Nyquist.
///
/// # Examples
///
/// ```
/// use astrs_nodes_signal::{design_lowpass, WindowFunction};
///
/// let taps = design_lowpass(31, 1_000.0, 8_000.0, WindowFunction::Hamming)?;
/// let dc_gain: f64 = taps.iter().sum();
/// assert!((dc_gain - 1.0).abs() < 1e-9);
/// # Ok::<(), astrs_nodes_signal::SignalError>(())
/// ```
pub fn design_lowpass(
    num_taps: usize,
    cutoff_hz: f64,
    sample_rate_hz: f64,
    window: WindowFunction,
) -> SignalResult<Vec<f64>> {
    validate_lowpass_inputs(num_taps, cutoff_hz, sample_rate_hz)?;
    let fc = cutoff_hz / sample_rate_hz;
    let center = (num_taps - 1) as f64 / 2.0;
    let taps: Vec<f64> = (0..num_taps)
        .map(|n| {
            let x = n as f64 - center;
            let ideal = 2.0 * fc * normalized_sinc(2.0 * fc * x);
            ideal * window.coefficient_symmetric(n, num_taps)
        })
        .collect();
    Ok(normalize_dc_gain(taps))
}

/// Scales `taps` so they sum to exactly `1.0` (unity DC gain) — a no-op
/// when the sum is already zero, which no valid lowpass design in this
/// module ever produces.
fn normalize_dc_gain(taps: Vec<f64>) -> Vec<f64> {
    let sum: f64 = taps.iter().sum();
    if sum.abs() < 1e-15 {
        return taps;
    }
    taps.into_iter().map(|h| h / sum).collect()
}

/// Designs a windowed-sinc highpass FIR filter via **spectral inversion** of
/// a [`design_lowpass`] prototype: `h_hp = delta - h_lp`, where `delta` is a
/// unit impulse at the center tap. Because `Σh_lp = 1`, `Σh_hp = 0` exactly
/// — zero gain at DC, unity gain at Nyquist.
///
/// Requires an **odd** `num_taps` so the center tap `(num_taps - 1) / 2` is
/// a single exact index (spectral inversion around a half-sample position
/// would not be a real, causal FIR filter).
///
/// # Errors
///
/// [`SignalError::EvenTapsForSpectralInversion`] if `num_taps` is even, or
/// whatever [`design_lowpass`] reports for the same `cutoff_hz`.
///
/// # Examples
///
/// ```
/// use astrs_nodes_signal::{design_highpass, WindowFunction};
///
/// let taps = design_highpass(31, 1_000.0, 8_000.0, WindowFunction::Hamming)?;
/// let dc_gain: f64 = taps.iter().sum();
/// assert!(dc_gain.abs() < 1e-9);
/// # Ok::<(), astrs_nodes_signal::SignalError>(())
/// ```
pub fn design_highpass(
    num_taps: usize,
    cutoff_hz: f64,
    sample_rate_hz: f64,
    window: WindowFunction,
) -> SignalResult<Vec<f64>> {
    if num_taps.is_multiple_of(2) {
        return Err(SignalError::EvenTapsForSpectralInversion { num_taps });
    }
    let lowpass = design_lowpass(num_taps, cutoff_hz, sample_rate_hz, window)?;
    Ok(spectral_invert(lowpass))
}

/// `delta - h`, `delta` a unit impulse at the center tap — the shared step
/// behind [`design_highpass`] and [`design_bandstop`].
fn spectral_invert(taps: Vec<f64>) -> Vec<f64> {
    let center = (taps.len() - 1) / 2;
    let mut inverted: Vec<f64> = taps.into_iter().map(|h| -h).collect();
    inverted[center] += 1.0;
    inverted
}

/// Designs a windowed-sinc bandpass FIR filter passing `[low_hz, high_hz]`,
/// as the difference of two [`design_lowpass`] prototypes sharing the same
/// tap count and window: `h_bp = h_lp(high_hz) - h_lp(low_hz)`.
///
/// # Errors
///
/// [`SignalError::InvalidBand`] unless `0 < low_hz < high_hz < Nyquist`, or
/// whatever [`design_lowpass`] reports.
///
/// # Examples
///
/// ```
/// use astrs_nodes_signal::{design_bandpass, WindowFunction};
///
/// let taps = design_bandpass(63, 500.0, 1_500.0, 8_000.0, WindowFunction::Hamming)?;
/// assert_eq!(taps.len(), 63);
/// # Ok::<(), astrs_nodes_signal::SignalError>(())
/// ```
pub fn design_bandpass(
    num_taps: usize,
    low_hz: f64,
    high_hz: f64,
    sample_rate_hz: f64,
    window: WindowFunction,
) -> SignalResult<Vec<f64>> {
    let nyquist_hz = sample_rate_hz / 2.0;
    if !(low_hz.is_finite()
        && high_hz.is_finite()
        && 0.0 < low_hz
        && low_hz < high_hz
        && high_hz < nyquist_hz)
    {
        return Err(SignalError::InvalidBand {
            low_hz,
            high_hz,
            nyquist_hz,
        });
    }
    let lp_high = design_lowpass(num_taps, high_hz, sample_rate_hz, window)?;
    let lp_low = design_lowpass(num_taps, low_hz, sample_rate_hz, window)?;
    Ok(lp_high
        .into_iter()
        .zip(lp_low)
        .map(|(high, low)| high - low)
        .collect())
}

/// Designs a windowed-sinc bandstop (notch) FIR filter rejecting
/// `[low_hz, high_hz]`, via spectral inversion of [`design_bandpass`] —
/// the same trick [`design_highpass`] applies to [`design_lowpass`].
///
/// # Errors
///
/// [`SignalError::EvenTapsForSpectralInversion`] if `num_taps` is even, or
/// whatever [`design_bandpass`] reports.
pub fn design_bandstop(
    num_taps: usize,
    low_hz: f64,
    high_hz: f64,
    sample_rate_hz: f64,
    window: WindowFunction,
) -> SignalResult<Vec<f64>> {
    if num_taps.is_multiple_of(2) {
        return Err(SignalError::EvenTapsForSpectralInversion { num_taps });
    }
    let bandpass = design_bandpass(num_taps, low_hz, high_hz, sample_rate_hz, window)?;
    Ok(spectral_invert(bandpass))
}

/// A streaming direct-form FIR filter: `y[n] = Σ_{i=0}^{N-1} h[i]*x[n-i]`.
///
/// Keeps a delay line of the last `N - 1` input samples across calls, so
/// [`FirFilter::process`] is genuine **streaming convolution** — splitting
/// one input into several calls gives the same output as one call on the
/// concatenation (this module's own tests check that identity directly),
/// and used exactly that way by [`FirOperator`], which sees one [`Frame`]
/// per event rather than the whole signal at once.
#[derive(Debug, Clone)]
pub struct FirFilter {
    taps: Vec<f64>,
    /// The last `taps.len() - 1` inputs, oldest at the front.
    history: VecDeque<f64>,
}

impl FirFilter {
    /// Builds a filter from already-designed taps.
    ///
    /// # Errors
    ///
    /// [`SignalError::NoTaps`] if `taps` is empty.
    pub fn new(taps: Vec<f64>) -> SignalResult<Self> {
        if taps.is_empty() {
            return Err(SignalError::NoTaps { num_taps: 0 });
        }
        let history = VecDeque::from(vec![0.0; taps.len() - 1]);
        Ok(Self { taps, history })
    }

    /// This filter's taps, `h[0]` (applied to the newest sample) first.
    #[must_use]
    pub fn taps(&self) -> &[f64] {
        &self.taps
    }

    /// Clears the delay line, as if this filter had never seen a sample.
    pub fn reset(&mut self) {
        for slot in &mut self.history {
            *slot = 0.0;
        }
    }

    /// Filters one sample, advancing the delay line by one.
    pub fn process_sample(&mut self, x: f64) -> f64 {
        let mut y = self.taps[0] * x;
        for (i, &past) in self.history.iter().rev().enumerate() {
            y += self.taps[i + 1] * past;
        }
        self.history.push_back(x);
        self.history.pop_front();
        y
    }

    /// Filters `input`, continuing this filter's delay line across the call
    /// boundary (see the [struct documentation](Self) for why that matters).
    pub fn process(&mut self, input: &[f64]) -> Vec<f64> {
        input.iter().map(|&x| self.process_sample(x)).collect()
    }
}

/// Applies a windowed-sinc FIR filter to each incoming [`Frame`].
///
/// # Configuration
///
/// ```yaml
/// operators:
///   - id: anti-alias
///     operator: FirOperator
///     config:
///       shape: "lowpass"        # lowpass | highpass | bandpass | bandstop
///       num_taps: 63
///       sample_rate_hz: 48000.0
///       cutoff_hz: 4000.0        # lowpass/highpass
///       # low_hz / high_hz instead, for bandpass/bandstop
///       window: "hamming"        # optional, defaults to hamming
/// ```
#[operator]
#[derive(Debug, Default)]
pub struct FirOperator {
    filter: Option<FirFilter>,
}

/// The classic windowed-sinc textbook default (~53 dB stopband
/// attenuation) — used whenever `config.window` is omitted.
const DEFAULT_FIR_WINDOW: WindowFunction = WindowFunction::Hamming;

impl Operator for FirOperator {
    fn configure(&mut self, config: &BTreeMap<String, Parameter>) -> OpResult<()> {
        let Some(Parameter::String(shape_name)) = config.get("shape") else {
            return Err(OpError::failed(
                "FirOperator requires a string \"shape\" (lowpass, highpass, bandpass, bandstop)",
            ));
        };
        let num_taps = match config.get("num_taps") {
            Some(Parameter::Integer(value)) if *value > 0 => *value as usize,
            Some(other) => {
                return Err(OpError::failed(format!(
                    "\"num_taps\" must be a positive integer, got {other:?}"
                )));
            }
            None => return Err(OpError::failed("FirOperator requires \"num_taps\"")),
        };
        let sample_rate_hz = read_float(config, "sample_rate_hz")?;
        let window = config
            .get("window")
            .map(|value| match value {
                Parameter::String(name) => WindowFunction::parse(name)
                    .ok_or_else(|| OpError::failed(format!("unknown window {name:?}"))),
                other => Err(OpError::failed(format!(
                    "\"window\" must be a string, got {other:?}"
                ))),
            })
            .transpose()?
            .unwrap_or(DEFAULT_FIR_WINDOW);

        let taps = match shape_name.as_str() {
            "lowpass" => {
                let cutoff_hz = read_float(config, "cutoff_hz")?;
                design_lowpass(num_taps, cutoff_hz, sample_rate_hz, window)
            }
            "highpass" => {
                let cutoff_hz = read_float(config, "cutoff_hz")?;
                design_highpass(num_taps, cutoff_hz, sample_rate_hz, window)
            }
            "bandpass" => {
                let low_hz = read_float(config, "low_hz")?;
                let high_hz = read_float(config, "high_hz")?;
                design_bandpass(num_taps, low_hz, high_hz, sample_rate_hz, window)
            }
            "bandstop" => {
                let low_hz = read_float(config, "low_hz")?;
                let high_hz = read_float(config, "high_hz")?;
                design_bandstop(num_taps, low_hz, high_hz, sample_rate_hz, window)
            }
            other => {
                return Err(OpError::failed(format!(
                    "unknown FIR shape {other:?}: expected lowpass, highpass, bandpass, bandstop"
                )));
            }
        }
        .map_err(|err| OpError::failed(err.to_string()))?;

        self.filter = Some(FirFilter::new(taps).map_err(|err| OpError::failed(err.to_string()))?);
        Ok(())
    }

    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input {
                metadata, payload, ..
            } => {
                let filter = self
                    .filter
                    .as_mut()
                    .ok_or_else(|| OpError::failed("FirOperator was never configured"))?;
                let frame: Frame = decode(payload)?;
                let filtered = filter.process(&frame.to_f64());
                out.send("filtered", metadata.clone(), &Frame::from_f64(&filtered))?;
                Ok(Status::Continue)
            }
            OpEvent::Stop { .. } => Ok(Status::Finished),
            _ => Ok(Status::Continue),
        }
    }

    fn on_reload(&mut self, _out: &mut OpOutput) -> OpResult<()> {
        if let Some(filter) = self.filter.as_mut() {
            filter.reset();
        }
        Ok(())
    }
}

/// Reads a required `Parameter::Float` field from a `configure()` map.
fn read_float(config: &BTreeMap<String, Parameter>, key: &str) -> OpResult<f64> {
    match config.get(key) {
        Some(Parameter::Float(value)) => Ok(*value),
        Some(other) => Err(OpError::failed(format!(
            "{key:?} must be a float, got {other:?}"
        ))),
        None => Err(OpError::failed(format!("FirOperator requires {key:?}"))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_operator_api::AstrsMessage;

    const FS: f64 = 8_000.0;

    fn close(left: f64, right: f64, tol: f64) -> bool {
        (left - right).abs() < tol
    }

    // ---- Design closed-form properties ----

    #[test]
    fn lowpass_has_exactly_unity_dc_gain() {
        let taps = design_lowpass(31, 1_000.0, FS, WindowFunction::Hamming).unwrap();
        let dc: f64 = taps.iter().sum();
        assert!(close(dc, 1.0, 1e-9));
    }

    #[test]
    fn highpass_has_exactly_zero_dc_gain() {
        let taps = design_highpass(31, 1_000.0, FS, WindowFunction::Hamming).unwrap();
        let dc: f64 = taps.iter().sum();
        assert!(dc.abs() < 1e-9);
    }

    #[test]
    fn highpass_rejects_an_even_tap_count() {
        assert_eq!(
            design_highpass(30, 1_000.0, FS, WindowFunction::Hamming).unwrap_err(),
            SignalError::EvenTapsForSpectralInversion { num_taps: 30 }
        );
    }

    #[test]
    fn bandpass_is_symmetric_and_rejects_a_backwards_band() {
        let taps = design_bandpass(63, 500.0, 1_500.0, FS, WindowFunction::Hamming).unwrap();
        assert_eq!(taps.len(), 63);
        for i in 0..taps.len() {
            assert!(close(taps[i], taps[taps.len() - 1 - i], 1e-9));
        }
        assert!(design_bandpass(63, 1_500.0, 500.0, FS, WindowFunction::Hamming).is_err());
    }

    #[test]
    fn bandstop_has_unity_dc_gain_and_rejects_an_even_tap_count() {
        let taps = design_bandstop(63, 500.0, 1_500.0, FS, WindowFunction::Hamming).unwrap();
        let dc: f64 = taps.iter().sum();
        assert!(close(dc, 1.0, 1e-9));
        assert!(design_bandstop(62, 500.0, 1_500.0, FS, WindowFunction::Hamming).is_err());
    }

    #[test]
    fn design_rejects_zero_taps_and_bad_cutoff() {
        assert_eq!(
            design_lowpass(0, 1_000.0, FS, WindowFunction::Hamming).unwrap_err(),
            SignalError::NoTaps { num_taps: 0 }
        );
        assert!(design_lowpass(31, 0.0, FS, WindowFunction::Hamming).is_err());
        assert!(design_lowpass(31, FS, FS, WindowFunction::Hamming).is_err());
    }

    // ---- FirFilter: impulse response identity ----

    /// An FIR filter's impulse response **is** its own taps, exactly — the
    /// hardest closed-form test this module has, and the one most likely to
    /// catch a delay-line indexing bug.
    #[test]
    fn impulse_response_equals_the_taps_exactly() {
        let taps = design_lowpass(15, 1_000.0, FS, WindowFunction::Hamming).unwrap();
        let mut filter = FirFilter::new(taps.clone()).unwrap();
        let mut impulse = vec![0.0; 20];
        impulse[0] = 1.0;
        let response = filter.process(&impulse);
        assert_eq!(&response[..15], taps.as_slice());
        assert!(response[15..].iter().all(|&y| y == 0.0));
    }

    // `FirFilter::process` split across a chunk boundary must equal one
    // call on the concatenation — the streaming-continuity guarantee.
    #[test]
    fn streaming_continuity_across_a_chunk_boundary() {
        let taps = design_lowpass(11, 1_000.0, FS, WindowFunction::Hamming).unwrap();
        let input: Vec<f64> = (0..40).map(|n| (n as f64 * 0.37).sin()).collect();

        let mut whole = FirFilter::new(taps.clone()).unwrap();
        let all_at_once = whole.process(&input);

        let mut chunked = FirFilter::new(taps).unwrap();
        let mut in_pieces = chunked.process(&input[..13]);
        in_pieces.extend(chunked.process(&input[13..27]));
        in_pieces.extend(chunked.process(&input[27..]));

        assert_eq!(all_at_once, in_pieces);
    }

    #[test]
    fn reset_clears_the_delay_line() {
        let taps = design_lowpass(9, 1_000.0, FS, WindowFunction::Hamming).unwrap();
        let mut filter = FirFilter::new(taps.clone()).unwrap();
        let mut impulse = vec![0.0; 12];
        impulse[0] = 1.0;
        let first = filter.process(&impulse);
        filter.reset();
        let second = filter.process(&impulse);
        assert_eq!(first, second);
    }

    #[test]
    fn new_rejects_empty_taps() {
        assert_eq!(
            FirFilter::new(Vec::new()).unwrap_err(),
            SignalError::NoTaps { num_taps: 0 }
        );
    }

    #[test]
    fn a_single_tap_filter_is_a_pure_gain() {
        let mut filter = FirFilter::new(vec![0.5]).unwrap();
        let out = filter.process(&[1.0, 2.0, 3.0]);
        assert_eq!(out, vec![0.5, 1.0, 1.5]);
    }

    // ---- FirOperator ----

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
    fn operator_configure_and_filter_a_frame() {
        let mut op = FirOperator::default();
        let mut config = BTreeMap::new();
        config.insert("shape".to_owned(), Parameter::String("lowpass".to_owned()));
        config.insert("num_taps".to_owned(), Parameter::Integer(15));
        config.insert("sample_rate_hz".to_owned(), Parameter::Float(FS));
        config.insert("cutoff_hz".to_owned(), Parameter::Float(1_000.0));
        op.configure(&config).unwrap();

        let mut out = OpOutput::new();
        let mut impulse = vec![0.0_f32; 20];
        impulse[0] = 1.0;
        let event = frame_event(&Frame::new(impulse));
        op.on_event(&event, &mut out).unwrap();
        let sends = out.drain();
        assert_eq!(sends[0].id().as_str(), "filtered");
        let decoded = astrs_data::ipc::decode_payload(sends[0].payload()).unwrap();
        let result = Frame::from_record_batch(&decoded).unwrap();
        assert_eq!(result.samples.len(), 20);
    }

    #[test]
    fn operator_rejects_an_unknown_shape() {
        let mut op = FirOperator::default();
        let mut config = BTreeMap::new();
        config.insert("shape".to_owned(), Parameter::String("nonsense".to_owned()));
        config.insert("num_taps".to_owned(), Parameter::Integer(15));
        config.insert("sample_rate_hz".to_owned(), Parameter::Float(FS));
        config.insert("cutoff_hz".to_owned(), Parameter::Float(1_000.0));
        assert!(op.configure(&config).is_err());
    }

    #[test]
    fn operator_rejects_input_before_configuration() {
        let mut op = FirOperator::default();
        let mut out = OpOutput::new();
        let err = op
            .on_event(&frame_event(&Frame::new(vec![0.0_f32])), &mut out)
            .unwrap_err();
        assert!(matches!(err, OpError::Failed { .. }));
    }

    #[test]
    fn operator_entry_name_is_snake_case() {
        let (name, _ctor) = FirOperator::operator_entry();
        assert_eq!(name, "fir_operator");
    }
}
