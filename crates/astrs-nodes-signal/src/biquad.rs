//! RBJ ("Audio EQ Cookbook") biquad filters and [`BiquadOperator`], the
//! astrs operator wrapping them.
//!
//! [`RbjCoefficients::design`] implements Robert Bristow-Johnson's widely
//! used "Cookbook formulae for audio equalizer biquad filter coefficients"
//! (the RBJ Audio EQ Cookbook), the reference every one of this module's
//! doc comments cites by section name. [`Biquad`] then realizes those
//! coefficients as a streaming Direct Form II Transposed second-order
//! section — the numerically well-behaved realization (only two state
//! variables, and unlike Direct Form I it does not need to keep the last two
//! *inputs* separately from the last two *outputs*) that most production
//! audio/DSP libraries use for exactly this filter shape.

use std::collections::BTreeMap;
use std::f64::consts::PI;

use astrs_operator_api::{OpError, OpEvent, OpOutput, OpResult, Operator, Status, operator};
use astrs_wire::Parameter;
use oxifft::Complex;

use crate::error::{SignalError, SignalResult};
use crate::message::Frame;
use crate::support::decode;

/// Which RBJ cookbook response [`RbjCoefficients::design`] builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BiquadKind {
    /// Passes frequencies below the cutoff; 0 dB gain at DC.
    LowPass,
    /// Passes frequencies above the cutoff; 0 dB gain at Nyquist.
    HighPass,
    /// Passes a band around the center frequency with **0 dB peak gain**
    /// (the RBJ cookbook's "constant 0 dB peak gain" BPF variant, as opposed
    /// to its "constant skirt gain" one) — exactly 0 dB at the center
    /// frequency itself.
    BandPass,
    /// Rejects a narrow band around the center frequency; 0 dB gain
    /// everywhere else, with an exact null at the center frequency.
    Notch,
}

impl BiquadKind {
    /// Parses a manifest-facing name, case-insensitively.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "lowpass" | "low_pass" | "lpf" => Some(Self::LowPass),
            "highpass" | "high_pass" | "hpf" => Some(Self::HighPass),
            "bandpass" | "band_pass" | "bpf" => Some(Self::BandPass),
            "notch" | "bandstop" | "band_stop" => Some(Self::Notch),
            _ => None,
        }
    }
}

/// Normalized second-order IIR coefficients: `a0` has already been divided
/// out, so the direct-form difference equation is
///
/// ```text
/// y[n] = b0*x[n] + b1*x[n-1] + b2*x[n-2] - a1*y[n-1] - a2*y[n-2]
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RbjCoefficients {
    /// Feed-forward tap on `x[n]`.
    pub b0: f64,
    /// Feed-forward tap on `x[n-1]`.
    pub b1: f64,
    /// Feed-forward tap on `x[n-2]`.
    pub b2: f64,
    /// Feed-back tap on `y[n-1]` (sign already folded into the difference
    /// equation above — this is the coefficient to *subtract*).
    pub a1: f64,
    /// Feed-back tap on `y[n-2]`.
    pub a2: f64,
}

impl RbjCoefficients {
    /// Designs an RBJ cookbook biquad.
    ///
    /// `frequency_hz` is the cutoff (lowpass/highpass) or center
    /// (bandpass/notch) frequency; `q` is the RBJ cookbook's own `Q`
    /// (`1/sqrt(2) ≈ 0.7071` gives a maximally-flat, Butterworth-like
    /// lowpass/highpass response; a larger `Q` narrows a bandpass/notch).
    ///
    /// Reference (RBJ Audio EQ Cookbook, common variables section):
    ///
    /// ```text
    /// w0    = 2*pi*f0/Fs
    /// alpha = sin(w0) / (2*Q)
    /// ```
    ///
    /// and, per kind (cookbook §"Simple filters" / "Band pass filter" /
    /// "Notch filter", `a0` normalized out of every coefficient below):
    ///
    /// ```text
    /// LPF:  b0=(1-cos w0)/2   b1=1-cos w0    b2=(1-cos w0)/2
    /// HPF:  b0=(1+cos w0)/2   b1=-(1+cos w0) b2=(1+cos w0)/2
    /// BPF:  b0=alpha          b1=0           b2=-alpha        (0 dB peak gain)
    /// Notch:b0=1              b1=-2 cos w0   b2=1
    /// every kind: a0=1+alpha  a1=-2 cos w0   a2=1-alpha
    /// ```
    ///
    /// # Errors
    ///
    /// [`SignalError::InvalidSampleRate`] if `sample_rate_hz` is not
    /// positive and finite, [`SignalError::CutoffOutOfRange`] if
    /// `frequency_hz` is not strictly between DC and Nyquist, or
    /// [`SignalError::NonPositiveQ`] if `q` is not positive and finite.
    pub fn design(
        kind: BiquadKind,
        sample_rate_hz: f64,
        frequency_hz: f64,
        q: f64,
    ) -> SignalResult<Self> {
        if !(sample_rate_hz.is_finite() && sample_rate_hz > 0.0) {
            return Err(SignalError::InvalidSampleRate { sample_rate_hz });
        }
        let nyquist_hz = sample_rate_hz / 2.0;
        if !(frequency_hz.is_finite() && frequency_hz > 0.0 && frequency_hz < nyquist_hz) {
            return Err(SignalError::CutoffOutOfRange {
                cutoff_hz: frequency_hz,
                nyquist_hz,
            });
        }
        if !(q.is_finite() && q > 0.0) {
            return Err(SignalError::NonPositiveQ { q });
        }

        let w0 = 2.0 * PI * frequency_hz / sample_rate_hz;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);

        let (b0, b1, b2, a0, a1, a2) = match kind {
            BiquadKind::LowPass => (
                (1.0 - cos_w0) / 2.0,
                1.0 - cos_w0,
                (1.0 - cos_w0) / 2.0,
                1.0 + alpha,
                -2.0 * cos_w0,
                1.0 - alpha,
            ),
            BiquadKind::HighPass => (
                (1.0 + cos_w0) / 2.0,
                -(1.0 + cos_w0),
                (1.0 + cos_w0) / 2.0,
                1.0 + alpha,
                -2.0 * cos_w0,
                1.0 - alpha,
            ),
            BiquadKind::BandPass => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos_w0, 1.0 - alpha),
            BiquadKind::Notch => (
                1.0,
                -2.0 * cos_w0,
                1.0,
                1.0 + alpha,
                -2.0 * cos_w0,
                1.0 - alpha,
            ),
        };

        Ok(Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        })
    }

    /// [`RbjCoefficients::design`] with [`BiquadKind::LowPass`].
    ///
    /// # Errors
    ///
    /// As [`RbjCoefficients::design`].
    pub fn lowpass(sample_rate_hz: f64, cutoff_hz: f64, q: f64) -> SignalResult<Self> {
        Self::design(BiquadKind::LowPass, sample_rate_hz, cutoff_hz, q)
    }

    /// [`RbjCoefficients::design`] with [`BiquadKind::HighPass`].
    ///
    /// # Errors
    ///
    /// As [`RbjCoefficients::design`].
    pub fn highpass(sample_rate_hz: f64, cutoff_hz: f64, q: f64) -> SignalResult<Self> {
        Self::design(BiquadKind::HighPass, sample_rate_hz, cutoff_hz, q)
    }

    /// [`RbjCoefficients::design`] with [`BiquadKind::BandPass`].
    ///
    /// # Errors
    ///
    /// As [`RbjCoefficients::design`].
    pub fn bandpass(sample_rate_hz: f64, center_hz: f64, q: f64) -> SignalResult<Self> {
        Self::design(BiquadKind::BandPass, sample_rate_hz, center_hz, q)
    }

    /// [`RbjCoefficients::design`] with [`BiquadKind::Notch`].
    ///
    /// # Errors
    ///
    /// As [`RbjCoefficients::design`].
    pub fn notch(sample_rate_hz: f64, center_hz: f64, q: f64) -> SignalResult<Self> {
        Self::design(BiquadKind::Notch, sample_rate_hz, center_hz, q)
    }

    /// The complex frequency response `H(e^{jw})` at `frequency_hz`,
    /// evaluated directly from the z-transform of the difference equation:
    ///
    /// ```text
    /// H(z) = (b0 + b1*z^-1 + b2*z^-2) / (1 + a1*z^-1 + a2*z^-2),  z = e^{jw}
    /// ```
    #[must_use]
    pub fn response(&self, sample_rate_hz: f64, frequency_hz: f64) -> Complex<f64> {
        let w = 2.0 * PI * frequency_hz / sample_rate_hz;
        let z_inv = Complex::from_polar(1.0, -w);
        let z_inv2 = z_inv * z_inv;
        let numerator = Complex::new(self.b0, 0.0) + z_inv.scale(self.b1) + z_inv2.scale(self.b2);
        let denominator = Complex::new(1.0, 0.0) + z_inv.scale(self.a1) + z_inv2.scale(self.a2);
        numerator / denominator
    }

    /// `|H(e^{jw})|` at `frequency_hz` — the linear-scale gain a pure tone
    /// at that frequency is scaled by.
    #[must_use]
    pub fn magnitude_at(&self, sample_rate_hz: f64, frequency_hz: f64) -> f64 {
        self.response(sample_rate_hz, frequency_hz).norm()
    }
}

/// A streaming second-order IIR section realizing [`RbjCoefficients`] as
/// Direct Form II Transposed:
///
/// ```text
/// y[n] = b0*x[n] + s1[n-1]
/// s1[n] = b1*x[n] - a1*y[n] + s2[n-1]
/// s2[n] = b2*x[n] - a2*y[n]
/// ```
///
/// State is always kept as `f64` regardless of which precision
/// [`Biquad::process_f32`]/[`Biquad::process_f64`] is called with — a
/// second-order recursive filter's own feedback compounds rounding error
/// over time, so computing the recursion in the wider type and narrowing
/// only the returned sample is a deliberate accuracy choice, not an
/// unfinished `f32` path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    coefficients: RbjCoefficients,
    state1: f64,
    state2: f64,
}

impl Biquad {
    /// Builds a fresh (zero-state) filter from already-designed coefficients.
    #[must_use]
    pub fn new(coefficients: RbjCoefficients) -> Self {
        Self {
            coefficients,
            state1: 0.0,
            state2: 0.0,
        }
    }

    /// This filter's coefficients.
    #[must_use]
    pub const fn coefficients(&self) -> RbjCoefficients {
        self.coefficients
    }

    /// Clears the filter's internal state, as if it had never seen a sample.
    pub fn reset(&mut self) {
        self.state1 = 0.0;
        self.state2 = 0.0;
    }

    /// Filters one `f64` sample.
    pub fn process_f64(&mut self, x: f64) -> f64 {
        let c = &self.coefficients;
        let y = c.b0.mul_add(x, self.state1);
        self.state1 = c.b1.mul_add(x, self.state2) - c.a1 * y;
        self.state2 = c.b2 * x - c.a2 * y;
        y
    }

    /// Filters one `f32` sample (computed in `f64`, narrowed on return).
    pub fn process_f32(&mut self, x: f32) -> f32 {
        self.process_f64(f64::from(x)) as f32
    }

    /// Filters `input` in place order, continuing this filter's state across
    /// the call boundary — the "streaming" half of a biquad: calling this
    /// twice on two halves of a signal gives the same result as one call on
    /// the concatenation.
    pub fn process_block_f64(&mut self, input: &[f64]) -> Vec<f64> {
        input.iter().map(|&x| self.process_f64(x)).collect()
    }

    /// The `f32` counterpart of [`Biquad::process_block_f64`].
    pub fn process_block_f32(&mut self, input: &[f32]) -> Vec<f32> {
        input.iter().map(|&x| self.process_f32(x)).collect()
    }
}

/// Applies an RBJ biquad filter to each incoming [`Frame`].
///
/// # Configuration
///
/// ```yaml
/// operators:
///   - id: hum-notch
///     operator: BiquadOperator
///     config:
///       kind: "notch"          # lowpass | highpass | bandpass | notch
///       sample_rate_hz: 48000.0
///       frequency_hz: 60.0
///       q: 8.0                 # optional, defaults to 1/sqrt(2)
/// ```
#[operator]
#[derive(Debug, Default)]
pub struct BiquadOperator {
    filter: Option<Biquad>,
}

/// The RBJ cookbook's own suggestion for a maximally-flat (Butterworth-like)
/// lowpass/highpass response, used whenever `config.q` is omitted.
const DEFAULT_Q: f64 = std::f64::consts::FRAC_1_SQRT_2;

impl Operator for BiquadOperator {
    fn configure(&mut self, config: &BTreeMap<String, Parameter>) -> OpResult<()> {
        let Some(Parameter::String(kind_name)) = config.get("kind") else {
            return Err(OpError::failed(
                "BiquadOperator requires a string \"kind\" (lowpass, highpass, bandpass, notch)",
            ));
        };
        let kind = BiquadKind::parse(kind_name)
            .ok_or_else(|| OpError::failed(format!("unknown biquad kind {kind_name:?}")))?;

        let sample_rate_hz = read_float(config, "sample_rate_hz")?;
        let frequency_hz = read_float(config, "frequency_hz")?;
        let q = config
            .get("q")
            .map(|value| match value {
                Parameter::Float(q) => Ok(*q),
                other => Err(OpError::failed(format!(
                    "\"q\" must be a float, got {other:?}"
                ))),
            })
            .transpose()?
            .unwrap_or(DEFAULT_Q);

        let coefficients = RbjCoefficients::design(kind, sample_rate_hz, frequency_hz, q)
            .map_err(|err| OpError::failed(err.to_string()))?;
        self.filter = Some(Biquad::new(coefficients));
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
                    .ok_or_else(|| OpError::failed("BiquadOperator was never configured"))?;
                let frame: Frame = decode(payload)?;
                let filtered = filter.process_block_f32(&frame.samples);
                out.send("filtered", metadata.clone(), &Frame::new(filtered))?;
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
        None => Err(OpError::failed(format!("BiquadOperator requires {key:?}"))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_operator_api::AstrsMessage;

    const FS: f64 = 48_000.0;

    fn close(left: f64, right: f64, tol: f64) -> bool {
        (left - right).abs() < tol
    }

    // ---- Closed-form magnitude identities (RBJ cookbook, exact by
    // ---- construction — see this module's derivations in the doc
    // ---- comments) ----

    #[test]
    fn lowpass_has_exactly_unity_gain_at_dc() {
        let c = RbjCoefficients::lowpass(FS, 1_000.0, DEFAULT_Q).unwrap();
        assert!(close(c.magnitude_at(FS, 1e-9), 1.0, 1e-9));
    }

    #[test]
    fn highpass_has_exactly_unity_gain_at_nyquist() {
        let c = RbjCoefficients::highpass(FS, 1_000.0, DEFAULT_Q).unwrap();
        assert!(close(c.magnitude_at(FS, FS / 2.0 - 1e-6), 1.0, 1e-6));
    }

    #[test]
    fn bandpass_zero_db_peak_gain_is_exactly_unity_at_the_center() {
        let c = RbjCoefficients::bandpass(FS, 1_000.0, 2.0).unwrap();
        assert!(close(c.magnitude_at(FS, 1_000.0), 1.0, 1e-9));
    }

    #[test]
    fn notch_nulls_exactly_at_the_center_and_passes_dc_and_nyquist() {
        let c = RbjCoefficients::notch(FS, 1_000.0, 8.0).unwrap();
        assert!(c.magnitude_at(FS, 1_000.0) < 1e-9, "center should null");
        assert!(close(c.magnitude_at(FS, 1e-9), 1.0, 1e-9));
        assert!(close(c.magnitude_at(FS, FS / 2.0 - 1e-6), 1.0, 1e-6));
    }

    /// The RBJ lowpass's own **magnitude at its cutoff frequency** — not
    /// merely its unity-gain-at-DC identity above — has a closed form too:
    /// `|H(e^{jw0})| == Q`, exactly, for *any* `Q`, not only the
    /// Butterworth `Q = 1/sqrt(2)` case where that happens to read as the
    /// textbook "-3 dB point". Derivable from `H(z)` at `z = e^{-jw0}`
    /// (`alpha = sin(w0)/(2Q)` cancels the `cos(w0)` terms in both the
    /// numerator and denominator down to a single surviving `alpha`-only
    /// ratio); confirmed numerically here across a `Q` sweep rather than
    /// asserted on faith in the derivation alone.
    #[test]
    fn lowpass_magnitude_at_cutoff_equals_q_for_any_q() {
        for &q in &[0.3, 0.5, DEFAULT_Q, 1.0, 2.0, 5.0] {
            let c = RbjCoefficients::lowpass(FS, 1_000.0, q).unwrap();
            assert!(
                close(c.magnitude_at(FS, 1_000.0), q, 1e-9),
                "Q={q}: |H(w0)|={}",
                c.magnitude_at(FS, 1_000.0)
            );
        }
    }

    /// The highpass counterpart of
    /// [`lowpass_magnitude_at_cutoff_equals_q_for_any_q`]: the same `|H(e^{jw0})|
    /// == Q` identity holds for the RBJ highpass too, by the same
    /// `alpha`-survives-the-cancellation argument applied to its own
    /// `b`/`a` coefficients.
    #[test]
    fn highpass_magnitude_at_cutoff_equals_q_for_any_q() {
        for &q in &[0.3, 0.5, DEFAULT_Q, 1.0, 2.0, 5.0] {
            let c = RbjCoefficients::highpass(FS, 1_000.0, q).unwrap();
            assert!(
                close(c.magnitude_at(FS, 1_000.0), q, 1e-9),
                "Q={q}: |H(w0)|={}",
                c.magnitude_at(FS, 1_000.0)
            );
        }
    }

    // ---- Design validation ----

    #[test]
    fn design_rejects_out_of_range_cutoff() {
        assert_eq!(
            RbjCoefficients::lowpass(FS, 0.0, DEFAULT_Q).unwrap_err(),
            SignalError::CutoffOutOfRange {
                cutoff_hz: 0.0,
                nyquist_hz: FS / 2.0
            }
        );
        assert!(RbjCoefficients::lowpass(FS, FS / 2.0, DEFAULT_Q).is_err());
        assert!(RbjCoefficients::lowpass(FS, FS, DEFAULT_Q).is_err());
    }

    #[test]
    fn design_rejects_non_positive_q() {
        assert_eq!(
            RbjCoefficients::lowpass(FS, 1_000.0, 0.0).unwrap_err(),
            SignalError::NonPositiveQ { q: 0.0 }
        );
        assert!(RbjCoefficients::lowpass(FS, 1_000.0, -1.0).is_err());
    }

    #[test]
    fn design_rejects_a_bad_sample_rate() {
        assert_eq!(
            RbjCoefficients::lowpass(0.0, 1_000.0, DEFAULT_Q).unwrap_err(),
            SignalError::InvalidSampleRate {
                sample_rate_hz: 0.0
            }
        );
    }

    // ---- Impulse / step responses ----

    #[test]
    fn impulse_response_matches_a_hand_written_direct_form_recursion() {
        let c = RbjCoefficients::lowpass(FS, 1_000.0, DEFAULT_Q).unwrap();
        let mut biquad = Biquad::new(c);
        let mut impulse = vec![0.0; 32];
        impulse[0] = 1.0;
        let got = biquad.process_block_f64(&impulse);

        // Independent Direct-Form-I simulation of the same difference
        // equation, sharing no code with `Biquad::process_f64`.
        let mut x_hist = [0.0_f64; 2];
        let mut y_hist = [0.0_f64; 2];
        let mut expected = Vec::with_capacity(32);
        for &x in &impulse {
            let y = c.b0 * x + c.b1 * x_hist[0] + c.b2 * x_hist[1]
                - c.a1 * y_hist[0]
                - c.a2 * y_hist[1];
            x_hist[1] = x_hist[0];
            x_hist[0] = x;
            y_hist[1] = y_hist[0];
            y_hist[0] = y;
            expected.push(y);
        }
        for (a, b) in got.iter().zip(expected.iter()) {
            assert!(close(*a, *b, 1e-9), "{a} vs {b}");
        }
    }

    #[test]
    fn lowpass_step_response_converges_to_the_dc_gain() {
        let c = RbjCoefficients::lowpass(FS, 4_000.0, DEFAULT_Q).unwrap();
        let mut biquad = Biquad::new(c);
        let step = vec![1.0; 4_000];
        let response = biquad.process_block_f64(&step);
        let tail_average: f64 =
            response[3_900..].iter().sum::<f64>() / (response.len() - 3_900) as f64;
        assert!(close(tail_average, 1.0, 1e-3), "settled at {tail_average}");
    }

    #[test]
    fn highpass_step_response_decays_to_zero() {
        let c = RbjCoefficients::highpass(FS, 4_000.0, DEFAULT_Q).unwrap();
        let mut biquad = Biquad::new(c);
        let step = vec![1.0; 4_000];
        let response = biquad.process_block_f64(&step);
        let tail_average: f64 =
            response[3_900..].iter().sum::<f64>() / (response.len() - 3_900) as f64;
        assert!(tail_average.abs() < 1e-3, "settled at {tail_average}");
    }

    #[test]
    fn reset_clears_state_so_a_second_impulse_matches_the_first() {
        let c = RbjCoefficients::lowpass(FS, 1_000.0, DEFAULT_Q).unwrap();
        let mut biquad = Biquad::new(c);
        let mut impulse = vec![0.0; 8];
        impulse[0] = 1.0;
        let first = biquad.process_block_f64(&impulse);
        biquad.process_block_f64(&[0.0; 4]); // let the tail run on
        biquad.reset();
        let second = biquad.process_block_f64(&impulse);
        assert_eq!(first, second);
    }

    #[test]
    fn process_block_is_equivalent_to_repeated_process_calls() {
        let c = RbjCoefficients::lowpass(FS, 1_000.0, DEFAULT_Q).unwrap();
        let mut streaming = Biquad::new(c);
        let mut blocked = Biquad::new(c);
        let input: Vec<f64> = (0..16).map(|n| (n as f64 * 0.3).sin()).collect();

        let mut streamed = Vec::with_capacity(16);
        for &x in &input {
            streamed.push(streaming.process_f64(x));
        }
        // Split across two `process_block_f64` calls to exercise streaming
        // state across a chunk boundary, not just a single big call.
        let mut blocked_out = blocked.process_block_f64(&input[..7]);
        blocked_out.extend(blocked.process_block_f64(&input[7..]));

        assert_eq!(streamed, blocked_out);
    }

    #[test]
    fn f32_and_f64_processing_agree_within_f32_precision() {
        let c = RbjCoefficients::lowpass(FS, 1_000.0, DEFAULT_Q).unwrap();
        let mut f64_filter = Biquad::new(c);
        let mut f32_filter = Biquad::new(c);
        let input_f64: Vec<f64> = (0..64).map(|n| (n as f64 * 0.2).sin()).collect();
        let input_f32: Vec<f32> = input_f64.iter().map(|&x| x as f32).collect();

        let out_f64 = f64_filter.process_block_f64(&input_f64);
        let out_f32 = f32_filter.process_block_f32(&input_f32);
        for (a, b) in out_f64.iter().zip(out_f32.iter()) {
            assert!((*a as f32 - *b).abs() < 1e-4, "{a} vs {b}");
        }
    }

    // ---- BiquadKind::parse ----

    #[test]
    fn biquad_kind_parse_recognizes_every_spelling() {
        assert_eq!(BiquadKind::parse("LowPass"), Some(BiquadKind::LowPass));
        assert_eq!(BiquadKind::parse("hpf"), Some(BiquadKind::HighPass));
        assert_eq!(BiquadKind::parse("band_pass"), Some(BiquadKind::BandPass));
        assert_eq!(BiquadKind::parse("bandstop"), Some(BiquadKind::Notch));
        assert_eq!(BiquadKind::parse("nonsense"), None);
    }

    // ---- BiquadOperator ----

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
    fn operator_rejects_input_before_configuration() {
        let mut op = BiquadOperator::default();
        let mut out = OpOutput::new();
        let event = frame_event(&Frame::new(vec![0.0_f32]));
        let err = op.on_event(&event, &mut out).unwrap_err();
        assert!(matches!(err, OpError::Failed { .. }));
    }

    #[test]
    fn operator_configure_requires_kind_and_frequency() {
        let mut op = BiquadOperator::default();
        assert!(op.configure(&BTreeMap::new()).is_err());

        let mut config = BTreeMap::new();
        config.insert("kind".to_owned(), Parameter::String("lowpass".to_owned()));
        assert!(
            op.configure(&config).is_err(),
            "missing sample_rate_hz/frequency_hz must be rejected"
        );
    }

    #[test]
    fn operator_filters_a_frame_end_to_end() {
        let mut op = BiquadOperator::default();
        let mut config = BTreeMap::new();
        config.insert("kind".to_owned(), Parameter::String("lowpass".to_owned()));
        config.insert("sample_rate_hz".to_owned(), Parameter::Float(FS));
        config.insert("frequency_hz".to_owned(), Parameter::Float(1_000.0));
        op.configure(&config).unwrap();

        let mut out = OpOutput::new();
        let mut impulse = vec![0.0_f32; 16];
        impulse[0] = 1.0;
        let event = frame_event(&Frame::new(impulse.clone()));
        op.on_event(&event, &mut out).unwrap();
        let sends = out.drain();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].id().as_str(), "filtered");
        let decoded = astrs_data::ipc::decode_payload(sends[0].payload()).unwrap();
        let result = Frame::from_record_batch(&decoded).unwrap();
        assert_eq!(result.samples.len(), 16);
        assert_ne!(
            result.samples, impulse,
            "the filter should have changed the impulse"
        );
    }

    #[test]
    fn operator_default_q_matches_the_cookbook_suggestion() {
        let mut op = BiquadOperator::default();
        let mut config = BTreeMap::new();
        config.insert("kind".to_owned(), Parameter::String("lowpass".to_owned()));
        config.insert("sample_rate_hz".to_owned(), Parameter::Float(FS));
        config.insert("frequency_hz".to_owned(), Parameter::Float(1_000.0));
        op.configure(&config).unwrap();
        let expected = RbjCoefficients::lowpass(FS, 1_000.0, DEFAULT_Q).unwrap();
        assert_eq!(op.filter.unwrap().coefficients(), expected);
    }

    #[test]
    fn operator_entry_name_is_snake_case() {
        let (name, _ctor) = BiquadOperator::operator_entry();
        assert_eq!(name, "biquad_operator");
    }
}
