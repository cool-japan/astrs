//! Polyphase decimation and interpolation, and their operators
//! ([`DecimateOperator`], [`InterpolateOperator`]).
//!
//! # Why polyphase
//!
//! The naive way to decimate by `M` is "filter, then keep every `M`-th
//! sample" — correct, but it spends a full `N`-tap dot product computing
//! `M-1` output samples out of every `M` that are immediately thrown away.
//! Polyphase decomposition (Crochiere & Rabiner 1983; Vaidyanathan,
//! *Multirate Systems and Filter Banks*, ch. 4) restructures the same
//! filter into `M` shorter sub-filters so **only** the samples that survive
//! decimation are ever computed. Interpolation is the dual trick for
//! upsampling: instead of stuffing `L-1` zeros between input samples and
//! running a full-rate filter over mostly zeros, `L` sub-filters run
//! directly on the low-rate input and each produces one of the `L`
//! high-rate outputs per input sample.
//!
//! # Decimation, derived
//!
//! Direct form (the ground truth this module's own tests cross-check
//! against): `v[n] = Σ_{i=0}^{N-1} h[i] x[n-i]`, `y[m] = v[mM]`. Substituting
//! `i = jM + k` (`k` the *phase*, `0 <= k < M`) splits the sum by phase:
//!
//! ```text
//! y[m] = Σ_{k=0}^{M-1} Σ_j e_k[j] x[(m-j)M - k],   e_k[j] := h[jM + k]
//! ```
//! The inner sum is an ordinary FIR convolution of `e_k` against the
//! subsequence `u_k[p] := x[pM - k]` — phase `k`'s own private, already
//! decimated view of the input, delayed by `k` samples. [`Decimator`] keeps
//! one [`FirFilter`] per phase and, since raw sample `x[t]` belongs to
//! phase `k = (M - t mod M) mod M` (solving `t = pM - k` for `k`), routes
//! each incoming sample to exactly one phase; output `y[m]` is ready — and
//! is emitted — the instant phase `0` (which always fires on a multiple of
//! `M`) receives its `m`-th sample, at which point every other phase
//! already holds its own most recent (necessarily causal, since it was
//! pushed no later than this instant) contribution.
//!
//! # Interpolation, derived
//!
//! Zero-stuffing `x` by `L` and filtering with `h'` (`h` scaled to DC gain
//! `L`, compensating for the zero-stuffed signal's `1/L` average amplitude)
//! gives `y[n] = Σ_i h'[i] x_up[n-i]`, nonzero exactly when `n - i ≡ 0 (mod
//! L)`. Writing `n = mL + r` and `i = r + jL`:
//!
//! ```text
//! y[mL + r] = Σ_j f_r[j] x[m - j],   f_r[j] := h'[r + jL] = L·h[r + jL]
//! ```
//! — an ordinary FIR convolution of phase `r`'s taps against the **plain**
//! (never upsampled) input, evaluated at the low-rate index `m`. Each
//! incoming low-rate sample drives all `L` phases, producing the `L`
//! high-rate outputs `y[mL+0..mL+L)` in one step — no zero-valued
//! multiply-adds anywhere. [`Interpolator`] keeps one [`FirFilter`] per
//! phase, each fed the identical input sequence (a small, deliberate
//! duplication of delay-line state across phases in exchange for reusing
//! [`FirFilter`]'s already-tested single-phase engine unchanged — the
//! computational saving polyphase interpolation exists for, avoiding
//! `L`-fold-wasted multiply-adds against zero-stuffed samples, is
//! unaffected by which phases happen to share a buffer).

use std::collections::BTreeMap;

use astrs_operator_api::{OpError, OpEvent, OpOutput, OpResult, Operator, Status, operator};
use astrs_wire::Parameter;

use crate::error::{SignalError, SignalResult};
use crate::fir::{FirFilter, design_lowpass};
use crate::message::Frame;
use crate::support::decode;
use crate::window::WindowFunction;

/// The `J = ceil(taps.len() / factor)` polyphase components of `taps` for a
/// **decimator**: `e_k[j] = taps[jM + k]` in each; too-short taps are
/// implicitly zero (see the module documentation).
fn decimator_phases(taps: &[f64], factor: usize) -> Vec<Vec<f64>> {
    let phase_len = taps.len().div_ceil(factor);
    (0..factor)
        .map(|k| {
            (0..phase_len)
                .map(|j| taps.get(j * factor + k).copied().unwrap_or(0.0))
                .collect()
        })
        .collect()
}

/// The polyphase components of `taps` for an **interpolator**: `f_r[j] =
/// factor * taps[r + jL]`, pre-scaled by `factor` to compensate for
/// zero-stuffing's amplitude loss (see the module documentation).
fn interpolator_phases(taps: &[f64], factor: usize) -> Vec<Vec<f64>> {
    let phase_len = taps.len().div_ceil(factor);
    let scale = factor as f64;
    (0..factor)
        .map(|r| {
            (0..phase_len)
                .map(|j| taps.get(r + j * factor).copied().unwrap_or(0.0) * scale)
                .collect()
        })
        .collect()
}

/// A streaming polyphase decimator: downsamples by an integer `factor`
/// while applying an anti-aliasing FIR filter, computing only the samples
/// that survive decimation. See the module documentation for the
/// derivation.
#[derive(Debug)]
pub struct Decimator {
    factor: usize,
    phases: Vec<FirFilter>,
    /// Each phase's most recently computed contribution — phase `k`'s slot
    /// holds the value from its own latest push, which may be from an
    /// earlier decimation cycle than the one currently completing (see the
    /// module documentation). Initialized to `0.0`, matching the
    /// implicit zero history every phase starts with.
    last_output: Vec<f64>,
    /// Count of raw input samples seen so far, used to route each one to
    /// its phase (`k = (factor - t % factor) % factor`).
    samples_seen: u64,
}

impl Decimator {
    /// Builds a decimator from an already-designed anti-aliasing filter.
    ///
    /// # Errors
    ///
    /// [`SignalError::ZeroFactor`] if `factor == 0`, or
    /// [`SignalError::NoTaps`] if `taps` is empty.
    pub fn new(factor: usize, taps: &[f64]) -> SignalResult<Self> {
        if factor == 0 {
            return Err(SignalError::ZeroFactor);
        }
        if taps.is_empty() {
            return Err(SignalError::NoTaps { num_taps: 0 });
        }
        let phases = decimator_phases(taps, factor)
            .into_iter()
            .map(FirFilter::new)
            .collect::<SignalResult<Vec<_>>>()?;
        Ok(Self {
            factor,
            last_output: vec![0.0; phases.len()],
            phases,
            samples_seen: 0,
        })
    }

    /// The decimation factor.
    #[must_use]
    pub const fn factor(&self) -> usize {
        self.factor
    }

    /// Feeds `input` through the decimator, continuing state across calls
    /// (splitting one input into several calls gives the same output as one
    /// call on the concatenation). Returns between `input.len() / factor`
    /// and `input.len() / factor + 1` samples, depending on where the
    /// decimation phase stood when this call started.
    pub fn process(&mut self, input: &[f64]) -> Vec<f64> {
        let factor = self.factor as u64;
        let mut output = Vec::with_capacity(input.len() / self.factor + 1);
        for &x in input {
            let phase = ((factor - self.samples_seen % factor) % factor) as usize;
            self.last_output[phase] = self.phases[phase].process_sample(x);
            self.samples_seen += 1;
            if phase == 0 {
                output.push(self.last_output.iter().sum());
            }
        }
        output
    }
}

/// A streaming polyphase interpolator: upsamples by an integer `factor`
/// while applying a reconstruction FIR filter, computing only real
/// multiply-adds (never against a zero-stuffed sample). See the module
/// documentation for the derivation.
#[derive(Debug)]
pub struct Interpolator {
    factor: usize,
    /// One independently-stated [`FirFilter`] per phase, all fed the same
    /// input sequence (see the module documentation for why sharing
    /// state across phases is not required for correctness or for the
    /// algorithm's efficiency).
    phases: Vec<FirFilter>,
}

impl Interpolator {
    /// Builds an interpolator from an already-designed reconstruction
    /// filter (**not** yet scaled by `factor` — this constructor does that).
    ///
    /// # Errors
    ///
    /// [`SignalError::ZeroFactor`] if `factor == 0`, or
    /// [`SignalError::NoTaps`] if `taps` is empty.
    pub fn new(factor: usize, taps: &[f64]) -> SignalResult<Self> {
        if factor == 0 {
            return Err(SignalError::ZeroFactor);
        }
        if taps.is_empty() {
            return Err(SignalError::NoTaps { num_taps: 0 });
        }
        let phases = interpolator_phases(taps, factor)
            .into_iter()
            .map(FirFilter::new)
            .collect::<SignalResult<Vec<_>>>()?;
        Ok(Self { factor, phases })
    }

    /// The interpolation factor.
    #[must_use]
    pub const fn factor(&self) -> usize {
        self.factor
    }

    /// Feeds `input` through the interpolator, continuing state across
    /// calls. Returns exactly `input.len() * factor` samples, `factor` of
    /// them (in phase order) per input sample.
    pub fn process(&mut self, input: &[f64]) -> Vec<f64> {
        let mut output = Vec::with_capacity(input.len() * self.factor);
        for &x in input {
            for phase in &mut self.phases {
                output.push(phase.process_sample(x));
            }
        }
        output
    }
}

/// Reads a required positive `Parameter::Integer` `factor` field.
fn read_factor(config: &BTreeMap<String, Parameter>) -> OpResult<usize> {
    match config.get("factor") {
        Some(Parameter::Integer(value)) if *value > 0 => Ok(*value as usize),
        Some(other) => Err(OpError::failed(format!(
            "\"factor\" must be a positive integer, got {other:?}"
        ))),
        None => Err(OpError::failed("requires \"factor\"")),
    }
}

/// Reads a required `Parameter::Float` field.
fn read_float(config: &BTreeMap<String, Parameter>, key: &str) -> OpResult<f64> {
    match config.get(key) {
        Some(Parameter::Float(value)) => Ok(*value),
        Some(other) => Err(OpError::failed(format!(
            "{key:?} must be a float, got {other:?}"
        ))),
        None => Err(OpError::failed(format!("requires {key:?}"))),
    }
}

/// Reads an optional `num_taps` field, defaulting to `31`.
fn read_num_taps(config: &BTreeMap<String, Parameter>) -> OpResult<usize> {
    match config.get("num_taps") {
        Some(Parameter::Integer(value)) if *value > 0 => Ok(*value as usize),
        Some(other) => Err(OpError::failed(format!(
            "\"num_taps\" must be a positive integer, got {other:?}"
        ))),
        None => Ok(31),
    }
}

/// Downsamples each incoming [`Frame`] by an integer factor, applying a
/// polyphase anti-aliasing filter designed at the new Nyquist frequency
/// (`sample_rate_hz / (2 * factor)`).
///
/// # Configuration
///
/// ```yaml
/// operators:
///   - id: downsample
///     operator: DecimateOperator
///     config:
///       factor: 4
///       sample_rate_hz: 48000.0
///       num_taps: 63    # optional, defaults to 31
/// ```
#[operator]
#[derive(Default)]
pub struct DecimateOperator {
    decimator: Option<Decimator>,
}

impl Operator for DecimateOperator {
    fn configure(&mut self, config: &BTreeMap<String, Parameter>) -> OpResult<()> {
        let factor = read_factor(config)?;
        let sample_rate_hz = read_float(config, "sample_rate_hz")?;
        let num_taps = read_num_taps(config)?;
        let cutoff_hz = sample_rate_hz / (2.0 * factor as f64);
        let taps = design_lowpass(num_taps, cutoff_hz, sample_rate_hz, WindowFunction::Hamming)
            .map_err(|err| OpError::failed(err.to_string()))?;
        self.decimator =
            Some(Decimator::new(factor, &taps).map_err(|err| OpError::failed(err.to_string()))?);
        Ok(())
    }

    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input {
                metadata, payload, ..
            } => {
                let decimator = self
                    .decimator
                    .as_mut()
                    .ok_or_else(|| OpError::failed("DecimateOperator was never configured"))?;
                let frame: Frame = decode(payload)?;
                let downsampled = decimator.process(&frame.to_f64());
                out.send(
                    "decimated",
                    metadata.clone(),
                    &Frame::from_f64(&downsampled),
                )?;
                Ok(Status::Continue)
            }
            OpEvent::Stop { .. } => Ok(Status::Finished),
            _ => Ok(Status::Continue),
        }
    }
}

/// Upsamples each incoming [`Frame`] by an integer factor, applying a
/// polyphase reconstruction filter designed at the original Nyquist
/// frequency (so every image copy introduced by upsampling is suppressed).
///
/// # Configuration
///
/// ```yaml
/// operators:
///   - id: upsample
///     operator: InterpolateOperator
///     config:
///       factor: 4
///       sample_rate_hz: 12000.0   # the *input* rate, before interpolation
///       num_taps: 63              # optional, defaults to 31
/// ```
#[operator]
#[derive(Default)]
pub struct InterpolateOperator {
    interpolator: Option<Interpolator>,
}

impl Operator for InterpolateOperator {
    fn configure(&mut self, config: &BTreeMap<String, Parameter>) -> OpResult<()> {
        let factor = read_factor(config)?;
        let sample_rate_hz = read_float(config, "sample_rate_hz")?;
        let num_taps = read_num_taps(config)?;
        // The reconstruction filter's cutoff is the *input* signal's own
        // Nyquist frequency, expressed against the *output* (high) rate —
        // see the module documentation's interpolation derivation.
        let taps = design_lowpass(
            num_taps,
            sample_rate_hz / 2.0,
            sample_rate_hz * factor as f64,
            WindowFunction::Hamming,
        )
        .map_err(|err| OpError::failed(err.to_string()))?;
        self.interpolator =
            Some(Interpolator::new(factor, &taps).map_err(|err| OpError::failed(err.to_string()))?);
        Ok(())
    }

    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input {
                metadata, payload, ..
            } => {
                let interpolator = self
                    .interpolator
                    .as_mut()
                    .ok_or_else(|| OpError::failed("InterpolateOperator was never configured"))?;
                let frame: Frame = decode(payload)?;
                let upsampled = interpolator.process(&frame.to_f64());
                out.send(
                    "interpolated",
                    metadata.clone(),
                    &Frame::from_f64(&upsampled),
                )?;
                Ok(Status::Continue)
            }
            OpEvent::Stop { .. } => Ok(Status::Finished),
            _ => Ok(Status::Continue),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_operator_api::AstrsMessage;

    /// The direct-form ground truth this module's own derivation claims to
    /// match: filter, then keep every `factor`-th sample.
    fn direct_form_decimate(taps: &[f64], factor: usize, input: &[f64]) -> Vec<f64> {
        let mut output = Vec::new();
        let mut m = 0usize;
        loop {
            let center = m * factor;
            if center >= input.len() {
                break;
            }
            let mut y = 0.0;
            for (i, &h) in taps.iter().enumerate() {
                if let Some(idx) = center.checked_sub(i) {
                    y += h * input[idx];
                }
            }
            output.push(y);
            m += 1;
        }
        output
    }

    /// A pseudo-random-looking but fully deterministic test signal.
    fn synthetic_signal(len: usize) -> Vec<f64> {
        (0..len)
            .map(|n| (n as f64 * 0.618_034).sin() + 0.3 * (n as f64 * 1.732).cos())
            .collect()
    }

    #[test]
    fn decimator_matches_direct_form_across_n_m_pairs_including_m_1_and_non_multiples() {
        // Per the plan: sweep (N, M) including the M=1 identity case and N
        // not a multiple of M, which is exactly where a phase-routing
        // off-by-one would show up.
        let cases: &[(usize, usize, usize)] = &[
            (16, 1, 50), // M=1: decimation-by-1 must equal plain filtering
            (16, 4, 50), // N a multiple of M
            (15, 4, 50), // N not a multiple of M
            (7, 3, 40),
            (1, 5, 30), // a single-tap "filter": pure downsampling
            (20, 6, 61),
            (10, 10, 33), // M == N
            (10, 13, 33), // M > N
        ];
        for &(num_taps, factor, input_len) in cases {
            let taps: Vec<f64> = (0..num_taps).map(|i| 1.0 / (i as f64 + 1.0)).collect();
            let input = synthetic_signal(input_len);

            let expected = direct_form_decimate(&taps, factor, &input);
            let mut decimator = Decimator::new(factor, &taps).unwrap();
            let got = decimator.process(&input);

            assert_eq!(
                got.len(),
                expected.len(),
                "N={num_taps} M={factor} len={input_len}: output length mismatch"
            );
            for (i, (a, b)) in got.iter().zip(expected.iter()).enumerate() {
                assert!(
                    (a - b).abs() < 1e-9,
                    "N={num_taps} M={factor} len={input_len} sample {i}: {a} vs {b}"
                );
            }
        }
    }

    #[test]
    fn decimator_streaming_is_continuous_across_a_chunk_boundary() {
        let taps: Vec<f64> = (0..17).map(|i| 1.0 / (i as f64 + 1.0)).collect();
        let input = synthetic_signal(53);

        let mut whole = Decimator::new(5, &taps).unwrap();
        let all_at_once = whole.process(&input);

        let mut chunked = Decimator::new(5, &taps).unwrap();
        let mut in_pieces = chunked.process(&input[..11]);
        in_pieces.extend(chunked.process(&input[11..37]));
        in_pieces.extend(chunked.process(&input[37..]));

        assert_eq!(all_at_once.len(), in_pieces.len());
        for (a, b) in all_at_once.iter().zip(in_pieces.iter()) {
            assert!((a - b).abs() < 1e-9);
        }
    }

    #[test]
    fn decimator_of_a_dc_lowpass_recovers_the_dc_level() {
        let taps = design_lowpass(63, 500.0, 8_000.0, WindowFunction::Hamming).unwrap();
        let mut decimator = Decimator::new(4, &taps).unwrap();
        let input = vec![2.0; 2_000];
        let output = decimator.process(&input);
        let tail_average: f64 = output[400..].iter().sum::<f64>() / (output.len() - 400) as f64;
        assert!(
            (tail_average - 2.0).abs() < 1e-3,
            "settled at {tail_average}"
        );
    }

    #[test]
    fn decimator_rejects_zero_factor_and_empty_taps() {
        assert_eq!(
            Decimator::new(0, &[1.0]).unwrap_err(),
            SignalError::ZeroFactor
        );
        assert_eq!(
            Decimator::new(2, &[]).unwrap_err(),
            SignalError::NoTaps { num_taps: 0 }
        );
    }

    /// The direct-form ground truth for interpolation: zero-stuff by
    /// `factor`, scale the taps by `factor`, and convolve.
    fn direct_form_interpolate(taps: &[f64], factor: usize, input: &[f64]) -> Vec<f64> {
        let scaled: Vec<f64> = taps.iter().map(|&h| h * factor as f64).collect();
        let mut upsampled = vec![0.0; input.len() * factor];
        for (m, &x) in input.iter().enumerate() {
            upsampled[m * factor] = x;
        }
        let mut output = Vec::with_capacity(upsampled.len());
        for n in 0..upsampled.len() {
            let mut y = 0.0;
            for (i, &h) in scaled.iter().enumerate() {
                if let Some(idx) = n.checked_sub(i) {
                    y += h * upsampled[idx];
                }
            }
            output.push(y);
        }
        output
    }

    #[test]
    fn interpolator_matches_direct_form_zero_stuff_and_filter() {
        let cases: &[(usize, usize, usize)] = &[
            (16, 1, 20),
            (16, 4, 20),
            (15, 4, 20),
            (7, 3, 15),
            (1, 5, 10),
            (20, 6, 12),
        ];
        for &(num_taps, factor, input_len) in cases {
            let taps: Vec<f64> = (0..num_taps).map(|i| 1.0 / (i as f64 + 1.0)).collect();
            let input = synthetic_signal(input_len);

            let expected = direct_form_interpolate(&taps, factor, &input);
            let mut interpolator = Interpolator::new(factor, &taps).unwrap();
            let got = interpolator.process(&input);

            assert_eq!(got.len(), input_len * factor);
            assert_eq!(got.len(), expected.len());
            for (i, (a, b)) in got.iter().zip(expected.iter()).enumerate() {
                assert!(
                    (a - b).abs() < 1e-9,
                    "N={num_taps} L={factor} len={input_len} sample {i}: {a} vs {b}"
                );
            }
        }
    }

    #[test]
    fn interpolator_streaming_is_continuous_across_a_chunk_boundary() {
        let taps: Vec<f64> = (0..17).map(|i| 1.0 / (i as f64 + 1.0)).collect();
        let input = synthetic_signal(23);

        let mut whole = Interpolator::new(3, &taps).unwrap();
        let all_at_once = whole.process(&input);

        let mut chunked = Interpolator::new(3, &taps).unwrap();
        let mut in_pieces = chunked.process(&input[..9]);
        in_pieces.extend(chunked.process(&input[9..]));

        assert_eq!(all_at_once, in_pieces);
    }

    /// Constant input's steady-state level survives interpolation — but
    /// only approximately: a windowed-sinc reconstruction filter is not an
    /// ideal brick wall, so a few percent of ripple is expected, not exact
    /// equality.
    #[test]
    fn interpolator_of_a_dc_signal_approximately_preserves_the_level() {
        let taps = design_lowpass(63, 500.0, 8_000.0, WindowFunction::Hamming).unwrap();
        let mut interpolator = Interpolator::new(4, &taps).unwrap();
        let input = vec![3.0; 500];
        let output = interpolator.process(&input);
        let tail_average: f64 = output[1_600..].iter().sum::<f64>() / (output.len() - 1_600) as f64;
        assert!(
            (tail_average - 3.0).abs() < 3.0 * 0.05,
            "settled at {tail_average}, expected close to 3.0"
        );
    }

    #[test]
    fn interpolator_rejects_zero_factor_and_empty_taps() {
        assert_eq!(
            Interpolator::new(0, &[1.0]).unwrap_err(),
            SignalError::ZeroFactor
        );
        assert_eq!(
            Interpolator::new(2, &[]).unwrap_err(),
            SignalError::NoTaps { num_taps: 0 }
        );
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
    fn decimate_operator_configure_and_run() {
        let mut op = DecimateOperator::default();
        let mut config = BTreeMap::new();
        config.insert("factor".to_owned(), Parameter::Integer(4));
        config.insert("sample_rate_hz".to_owned(), Parameter::Float(8_000.0));
        op.configure(&config).unwrap();

        let mut out = OpOutput::new();
        let signal = Frame::from_f64(&synthetic_signal(64));
        op.on_event(&frame_event(&signal), &mut out).unwrap();
        let sends = out.drain();
        assert_eq!(sends[0].id().as_str(), "decimated");
        let decoded = astrs_data::ipc::decode_payload(sends[0].payload()).unwrap();
        let result = Frame::from_record_batch(&decoded).unwrap();
        assert_eq!(result.samples.len(), 16);
    }

    #[test]
    fn interpolate_operator_configure_and_run() {
        let mut op = InterpolateOperator::default();
        let mut config = BTreeMap::new();
        config.insert("factor".to_owned(), Parameter::Integer(3));
        config.insert("sample_rate_hz".to_owned(), Parameter::Float(8_000.0));
        op.configure(&config).unwrap();

        let mut out = OpOutput::new();
        let signal = Frame::from_f64(&synthetic_signal(20));
        op.on_event(&frame_event(&signal), &mut out).unwrap();
        let sends = out.drain();
        assert_eq!(sends[0].id().as_str(), "interpolated");
        let decoded = astrs_data::ipc::decode_payload(sends[0].payload()).unwrap();
        let result = Frame::from_record_batch(&decoded).unwrap();
        assert_eq!(result.samples.len(), 60);
    }

    #[test]
    fn decimate_operator_rejects_input_before_configuration() {
        let mut op = DecimateOperator::default();
        let mut out = OpOutput::new();
        let err = op
            .on_event(&frame_event(&Frame::new(vec![0.0_f32])), &mut out)
            .unwrap_err();
        assert!(matches!(err, OpError::Failed { .. }));
    }

    #[test]
    fn operator_entry_names_are_snake_case() {
        assert_eq!(DecimateOperator::operator_entry().0, "decimate_operator");
        assert_eq!(
            InterpolateOperator::operator_entry().0,
            "interpolate_operator"
        );
    }
}
