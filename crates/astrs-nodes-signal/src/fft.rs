//! FFT/IFFT wrappers over `oxifft`, [`ComplexSpectrum`] (the wire shape a
//! complex vector crosses an operator boundary as), and four operators:
//! [`FftOperator`]/[`IfftOperator`] (complex-to-complex) and
//! [`RfftOperator`]/[`IrfftOperator`] (real-to-complex and back).
//!
//! # Normalization convention
//!
//! Matching both `oxifft`'s own free functions and the usual DSP textbook
//! convention: the forward transforms ([`fft_forward`], [`real_fft`]) are
//! **unnormalized** (`X[k] = Σ_n x[n] e^{-j2πkn/N}`); the inverse transforms
//! ([`fft_inverse`], [`real_ifft`]) divide by `N`, so
//! `fft_inverse(&fft_forward(&x)?)? == x` round-trips without the caller
//! applying any scaling of their own.
//!
//! # Never a silent zero spectrum
//!
//! `oxifft`'s own convenience functions (`oxifft::fft`, `::ifft`, ...) plan
//! with `if let Some(plan) = Plan::dft_1d(..) { plan.execute(..) }` and no
//! `else` — a transform size the planner cannot build a plan for silently
//! returns every bin as `0`. In practice this never happens with
//! `Flags::ESTIMATE` (the flag both `oxifft`'s convenience functions and
//! every wrapper here use): its final fallback, a size-only heuristic
//! selection with no `None` case of its own, always succeeds — `None` is
//! only possible under `Flags::WISDOM_ONLY`, which nothing in this crate
//! requests. Every function in this module still plans for itself and maps
//! a `None` to [`SignalError::UnsupportedTransformSize`] rather than
//! inheriting that silent-zero behavior, because the guarantee above is an
//! implementation detail of a dependency, not a contract this crate's own
//! callers should have to trust blindly.
//!
//! # Typed via `astrs-data` tensors
//!
//! [`ComplexSpectrum::real_view`]/[`ComplexSpectrum::imag_view`] turn a
//! spectrum's two flat `Vec<f32>` columns into checked, zero-copy
//! [`astrs_data::tensor::TensorView`]s — the same accessor
//! [`astrs_data::tensor::ImageView`] wraps around `std/media/v1/Image`.

use std::collections::BTreeMap;

use astrs_data::tensor::TensorView;
use astrs_data::{RecordBatch, Result as DataResult};
use astrs_node_api::message::FromPayload;
use astrs_operator_api::{
    AstrsMessage, OpError, OpEvent, OpOutput, OpResult, Operator, Status, operator,
};
use astrs_wire::Parameter;
use oxifft::{Complex, Direction, Flags, Float, Plan, RealPlan};

use crate::error::{SignalError, SignalResult};
use crate::message::Frame;
use crate::support::decode;

/// Forward complex DFT: `X[k] = Σ_n x[n] e^{-j2πkn/N}` (unnormalized).
///
/// # Errors
///
/// [`SignalError::EmptyInput`] if `input` is empty, or
/// [`SignalError::UnsupportedTransformSize`] (see the module
/// documentation for when that can actually happen).
///
/// # Examples
///
/// ```
/// use astrs_nodes_signal::{fft_forward, fft_inverse};
/// use oxifft::Complex;
///
/// let input: Vec<Complex<f64>> = (0..8).map(|n| Complex::new(n as f64, 0.0)).collect();
/// let spectrum = fft_forward(&input)?;
/// // DC bin is the sum of the input.
/// assert!((spectrum[0].re - 28.0).abs() < 1e-9);
/// let recovered = fft_inverse(&spectrum)?;
/// for (a, b) in input.iter().zip(recovered.iter()) {
///     assert!((a.re - b.re).abs() < 1e-9);
/// }
/// # Ok::<(), astrs_nodes_signal::SignalError>(())
/// ```
pub fn fft_forward<T: Float>(input: &[Complex<T>]) -> SignalResult<Vec<Complex<T>>> {
    let n = input.len();
    if n == 0 {
        return Err(SignalError::EmptyInput {
            what: "fft_forward",
        });
    }
    let plan = Plan::<T>::dft_1d(n, Direction::Forward, Flags::ESTIMATE)
        .ok_or(SignalError::UnsupportedTransformSize { size: n })?;
    let mut output = vec![Complex::zero(); n];
    plan.execute(input, &mut output);
    Ok(output)
}

/// Inverse complex DFT, normalized by `1/N` (see the module
/// documentation).
///
/// # Errors
///
/// As [`fft_forward`].
pub fn fft_inverse<T: Float>(input: &[Complex<T>]) -> SignalResult<Vec<Complex<T>>> {
    let n = input.len();
    if n == 0 {
        return Err(SignalError::EmptyInput {
            what: "fft_inverse",
        });
    }
    let plan = Plan::<T>::dft_1d(n, Direction::Backward, Flags::ESTIMATE)
        .ok_or(SignalError::UnsupportedTransformSize { size: n })?;
    let mut output = vec![Complex::zero(); n];
    plan.execute(input, &mut output);
    let scale = T::ONE / T::from_usize(n);
    for value in &mut output {
        *value = value.scale(scale);
    }
    Ok(output)
}

/// Forward real-to-complex FFT: `N` real samples in, `N/2 + 1` complex bins
/// out (the non-redundant half of a real signal's Hermitian-symmetric
/// spectrum), unnormalized.
///
/// # Errors
///
/// As [`fft_forward`].
pub fn real_fft<T: Float>(input: &[T]) -> SignalResult<Vec<Complex<T>>> {
    let n = input.len();
    if n == 0 {
        return Err(SignalError::EmptyInput { what: "real_fft" });
    }
    let plan = RealPlan::<T>::r2c_1d(n, Flags::ESTIMATE)
        .ok_or(SignalError::UnsupportedTransformSize { size: n })?;
    let mut output = vec![Complex::zero(); plan.complex_size()];
    plan.execute_r2c(input, &mut output);
    Ok(output)
}

/// Inverse complex-to-real FFT: `n/2 + 1` complex bins in, `n` real samples
/// out, normalized by `1/n`.
///
/// # Errors
///
/// [`SignalError::EmptyInput`] if `n == 0`,
/// [`SignalError::InconsistentIrfftLength`] if `input.len() != n/2 + 1`, or
/// [`SignalError::UnsupportedTransformSize`] (see the module
/// documentation).
pub fn real_ifft<T: Float>(input: &[Complex<T>], n: usize) -> SignalResult<Vec<T>> {
    if n == 0 {
        return Err(SignalError::EmptyInput { what: "real_ifft" });
    }
    let expected = n / 2 + 1;
    if input.len() != expected {
        return Err(SignalError::InconsistentIrfftLength {
            n,
            bins: input.len(),
            expected,
        });
    }
    let plan = RealPlan::<T>::c2r_1d(n, Flags::ESTIMATE)
        .ok_or(SignalError::UnsupportedTransformSize { size: n })?;
    let mut output = vec![T::ZERO; n];
    plan.execute_c2r(input, &mut output);
    Ok(output)
}

/// A complex-valued vector crossing an operator boundary: a frequency-domain
/// spectrum ([`RfftOperator`]/[`FftOperator`] output), or a time-domain
/// complex signal such as IQ samples ([`FftOperator`]/[`IfftOperator`]
/// input).
///
/// ```
/// use astrs_nodes_signal::ComplexSpectrum;
/// use oxifft::Complex;
///
/// let values = vec![Complex::new(1.0, 2.0), Complex::new(3.0, -4.0)];
/// let spectrum = ComplexSpectrum::from_complex(&values);
/// assert_eq!(spectrum.magnitude()[1], 5.0); // |3 - 4i| = 5
/// assert_eq!(spectrum.to_complex(), values);
/// ```
#[derive(Debug, Clone, PartialEq, AstrsMessage)]
#[astrs(urn = "std/signal/v1/ComplexSpectrum")]
pub struct ComplexSpectrum {
    /// The real part of each bin, in bin order.
    pub real: Vec<f32>,
    /// The imaginary part of each bin, in bin order.
    pub imag: Vec<f32>,
}

impl ComplexSpectrum {
    /// Builds a spectrum from `f64` complex values (the precision every
    /// operator in this module computes in internally, matching
    /// [`crate::biquad::Biquad`]'s own "compute wide, narrow at the wire"
    /// choice), narrowing each part to `f32`.
    #[must_use]
    pub fn from_complex(values: &[Complex<f64>]) -> Self {
        Self {
            real: values.iter().map(|c| c.re as f32).collect(),
            imag: values.iter().map(|c| c.im as f32).collect(),
        }
    }

    /// Widens this spectrum back to `f64` complex values, pairing `real[i]`
    /// with `imag[i]`. If the two columns have different lengths (never the
    /// case for a spectrum this crate produced itself), the shorter one
    /// wins and the excess of the longer one is ignored.
    #[must_use]
    pub fn to_complex(&self) -> Vec<Complex<f64>> {
        self.real
            .iter()
            .zip(&self.imag)
            .map(|(&re, &im)| Complex::new(f64::from(re), f64::from(im)))
            .collect()
    }

    /// The number of bins (the shorter of the two columns).
    #[must_use]
    pub fn len(&self) -> usize {
        self.real.len().min(self.imag.len())
    }

    /// Whether this spectrum carries no bins.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `|X[k]|` for each bin.
    #[must_use]
    pub fn magnitude(&self) -> Vec<f32> {
        self.real
            .iter()
            .zip(&self.imag)
            .map(|(&re, &im)| re.hypot(im))
            .collect()
    }

    /// `arg(X[k])` (radians) for each bin.
    #[must_use]
    pub fn phase(&self) -> Vec<f32> {
        self.real
            .iter()
            .zip(&self.imag)
            .map(|(&re, &im)| im.atan2(re))
            .collect()
    }

    /// A checked, zero-copy 1-D [`TensorView`] over the real column.
    ///
    /// # Errors
    ///
    /// Whatever [`TensorView::from_values`] reports — unreachable in
    /// practice since a 1-D shape always matches a flat `Vec`'s own length.
    pub fn real_view(&self) -> astrs_data::Result<TensorView<f32>> {
        TensorView::from_values(self.real.clone(), [self.real.len()])
    }

    /// The `imag`-column counterpart of [`ComplexSpectrum::real_view`].
    ///
    /// # Errors
    ///
    /// As [`ComplexSpectrum::real_view`].
    pub fn imag_view(&self) -> astrs_data::Result<TensorView<f32>> {
        TensorView::from_values(self.imag.clone(), [self.imag.len()])
    }
}

impl FromPayload for ComplexSpectrum {
    fn from_batch(batch: &RecordBatch) -> DataResult<Self> {
        <Self as AstrsMessage>::from_record_batch(batch)
    }
}

/// A single-slot transform plan cache, rebuilt only when the requested size
/// changes — every `*Operator` in this module keeps one instead of
/// re-planning on every event, since a dataflow stage typically sees the
/// same frame size on every call.
struct PlanCache<P> {
    size: usize,
    plan: P,
}

impl<P> PlanCache<P> {
    /// Returns the cached plan for `size`, rebuilding via `build` first if
    /// the cache is empty or was built for a different size.
    fn get_or_build(
        slot: &mut Option<Self>,
        size: usize,
        build: impl FnOnce(usize) -> Option<P>,
    ) -> OpResult<&P> {
        let stale = slot.as_ref().is_none_or(|cache| cache.size != size);
        if stale {
            let plan = build(size).ok_or_else(|| {
                OpError::failed(SignalError::UnsupportedTransformSize { size }.to_string())
            })?;
            *slot = Some(Self { size, plan });
        }
        match slot {
            Some(cache) => Ok(&cache.plan),
            None => Err(OpError::failed(
                "internal error: transform plan cache empty immediately after being built",
            )),
        }
    }
}

/// Forward complex-to-complex FFT of each incoming [`ComplexSpectrum`].
#[operator]
#[derive(Default)]
pub struct FftOperator {
    plan: Option<PlanCache<Plan<f64>>>,
}

impl Operator for FftOperator {
    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input {
                metadata, payload, ..
            } => {
                let input: ComplexSpectrum = decode(payload)?;
                let values = input.to_complex();
                if values.is_empty() {
                    return Err(OpError::failed(
                        SignalError::EmptyInput {
                            what: "FftOperator",
                        }
                        .to_string(),
                    ));
                }
                let plan = PlanCache::get_or_build(&mut self.plan, values.len(), |n| {
                    Plan::<f64>::dft_1d(n, Direction::Forward, Flags::ESTIMATE)
                })?;
                let mut spectrum = vec![Complex::zero(); values.len()];
                plan.execute(&values, &mut spectrum);
                out.send(
                    "spectrum",
                    metadata.clone(),
                    &ComplexSpectrum::from_complex(&spectrum),
                )?;
                Ok(Status::Continue)
            }
            OpEvent::Stop { .. } => Ok(Status::Finished),
            _ => Ok(Status::Continue),
        }
    }
}

/// Inverse complex-to-complex FFT of each incoming [`ComplexSpectrum`],
/// normalized by `1/N`.
#[operator]
#[derive(Default)]
pub struct IfftOperator {
    plan: Option<PlanCache<Plan<f64>>>,
}

impl Operator for IfftOperator {
    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input {
                metadata, payload, ..
            } => {
                let input: ComplexSpectrum = decode(payload)?;
                let values = input.to_complex();
                if values.is_empty() {
                    return Err(OpError::failed(
                        SignalError::EmptyInput {
                            what: "IfftOperator",
                        }
                        .to_string(),
                    ));
                }
                let n = values.len();
                let plan = PlanCache::get_or_build(&mut self.plan, n, |n| {
                    Plan::<f64>::dft_1d(n, Direction::Backward, Flags::ESTIMATE)
                })?;
                let mut result = vec![Complex::zero(); n];
                plan.execute(&values, &mut result);
                let scale = 1.0 / n as f64;
                for value in &mut result {
                    *value = value.scale(scale);
                }
                out.send(
                    "signal",
                    metadata.clone(),
                    &ComplexSpectrum::from_complex(&result),
                )?;
                Ok(Status::Continue)
            }
            OpEvent::Stop { .. } => Ok(Status::Finished),
            _ => Ok(Status::Continue),
        }
    }
}

/// Forward real-to-complex FFT of each incoming [`Frame`].
#[operator]
#[derive(Default)]
pub struct RfftOperator {
    plan: Option<PlanCache<RealPlan<f64>>>,
}

impl Operator for RfftOperator {
    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input {
                metadata, payload, ..
            } => {
                let frame: Frame = decode(payload)?;
                let samples = frame.to_f64();
                if samples.is_empty() {
                    return Err(OpError::failed(
                        SignalError::EmptyInput {
                            what: "RfftOperator",
                        }
                        .to_string(),
                    ));
                }
                let plan = PlanCache::get_or_build(&mut self.plan, samples.len(), |n| {
                    RealPlan::<f64>::r2c_1d(n, Flags::ESTIMATE)
                })?;
                let mut spectrum = vec![Complex::zero(); plan.complex_size()];
                plan.execute_r2c(&samples, &mut spectrum);
                out.send(
                    "spectrum",
                    metadata.clone(),
                    &ComplexSpectrum::from_complex(&spectrum),
                )?;
                Ok(Status::Continue)
            }
            OpEvent::Stop { .. } => Ok(Status::Finished),
            _ => Ok(Status::Continue),
        }
    }
}

/// Inverse complex-to-real FFT of each incoming [`ComplexSpectrum`].
///
/// # Configuration
///
/// ```yaml
/// operators:
///   - id: reconstruct
///     operator: IrfftOperator
///     config:
///       output_length: 512   # optional; defaults to 2*(bins - 1), the
///                             # even-length convention
/// ```
#[operator]
#[derive(Default)]
pub struct IrfftOperator {
    plan: Option<PlanCache<RealPlan<f64>>>,
    output_length: Option<usize>,
}

impl Operator for IrfftOperator {
    fn configure(&mut self, config: &BTreeMap<String, Parameter>) -> OpResult<()> {
        if let Some(Parameter::Integer(value)) = config.get("output_length") {
            if *value <= 0 {
                return Err(OpError::failed(format!(
                    "\"output_length\" must be positive, got {value}"
                )));
            }
            self.output_length = Some(*value as usize);
        }
        Ok(())
    }

    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input {
                metadata, payload, ..
            } => {
                let spectrum: ComplexSpectrum = decode(payload)?;
                let values = spectrum.to_complex();
                let n = self
                    .output_length
                    .unwrap_or_else(|| 2 * values.len().saturating_sub(1));
                if n == 0 {
                    return Err(OpError::failed(
                        SignalError::EmptyInput {
                            what: "IrfftOperator",
                        }
                        .to_string(),
                    ));
                }
                if values.len() != n / 2 + 1 {
                    return Err(OpError::failed(
                        SignalError::InconsistentIrfftLength {
                            n,
                            bins: values.len(),
                            expected: n / 2 + 1,
                        }
                        .to_string(),
                    ));
                }
                let plan = PlanCache::get_or_build(&mut self.plan, n, |n| {
                    RealPlan::<f64>::c2r_1d(n, Flags::ESTIMATE)
                })?;
                let mut signal = vec![0.0_f64; n];
                plan.execute_c2r(&values, &mut signal);
                out.send("signal", metadata.clone(), &Frame::from_f64(&signal))?;
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

    fn close(left: f64, right: f64, tol: f64) -> bool {
        (left - right).abs() < tol
    }

    // ---- Empirical size sweep: confirms oxifft actually plans and round
    // ---- trips correctly (not just "does not panic") across power-of-two,
    // ---- prime, and composite non-power-of-two sizes, in both precisions.

    #[test]
    fn complex_roundtrip_across_many_sizes_f64() {
        for &n in &[1usize, 2, 3, 5, 6, 7, 8, 12, 15, 16, 64, 100] {
            let input: Vec<Complex<f64>> = (0..n)
                .map(|k| Complex::new((k as f64 * 0.37).sin(), (k as f64 * 0.19).cos()))
                .collect();
            let spectrum = fft_forward(&input).unwrap();
            assert_eq!(spectrum.len(), n);
            let recovered = fft_inverse(&spectrum).unwrap();
            for (a, b) in input.iter().zip(recovered.iter()) {
                assert!(
                    close(a.re, b.re, 1e-8) && close(a.im, b.im, 1e-8),
                    "n={n}: {a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn complex_roundtrip_across_many_sizes_f32() {
        for &n in &[1usize, 2, 3, 5, 7, 8, 15, 16, 64] {
            let input: Vec<Complex<f32>> = (0..n)
                .map(|k| Complex::new((k as f32 * 0.37).sin(), (k as f32 * 0.19).cos()))
                .collect();
            let spectrum = fft_forward(&input).unwrap();
            let recovered = fft_inverse(&spectrum).unwrap();
            for (a, b) in input.iter().zip(recovered.iter()) {
                assert!((a.re - b.re).abs() < 1e-3, "n={n}: {a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn real_roundtrip_across_many_sizes() {
        for &n in &[1usize, 2, 3, 5, 6, 7, 8, 12, 15, 16, 64, 100] {
            let input: Vec<f64> = (0..n).map(|k| (k as f64 * 0.53).sin()).collect();
            let spectrum = real_fft(&input).unwrap();
            assert_eq!(spectrum.len(), n / 2 + 1);
            let recovered = real_ifft(&spectrum, n).unwrap();
            for (a, b) in input.iter().zip(recovered.iter()) {
                assert!(close(*a, *b, 1e-8), "n={n}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn empty_input_is_rejected_not_planned() {
        assert_eq!(
            fft_forward::<f64>(&[]).unwrap_err(),
            SignalError::EmptyInput {
                what: "fft_forward"
            }
        );
        assert_eq!(
            real_fft::<f64>(&[]).unwrap_err(),
            SignalError::EmptyInput { what: "real_fft" }
        );
        assert_eq!(
            real_ifft::<f64>(&[], 0).unwrap_err(),
            SignalError::EmptyInput { what: "real_ifft" }
        );
    }

    #[test]
    fn irfft_rejects_a_bin_count_inconsistent_with_n() {
        let bins = vec![Complex::new(0.0, 0.0); 3];
        assert_eq!(
            real_ifft(&bins, 10).unwrap_err(),
            SignalError::InconsistentIrfftLength {
                n: 10,
                bins: 3,
                expected: 6,
            }
        );
    }

    // ---- Closed-form cases ----

    #[test]
    fn the_impulse_spectrum_has_constant_unit_magnitude() {
        // DFT of a unit impulse is the all-ones sequence: exact.
        let n = 32;
        let mut impulse = vec![Complex::new(0.0, 0.0); n];
        impulse[0] = Complex::new(1.0, 0.0);
        let spectrum = fft_forward(&impulse).unwrap();
        for bin in &spectrum {
            assert!(close(bin.re, 1.0, 1e-9) && close(bin.im, 0.0, 1e-9));
        }
    }

    #[test]
    fn the_dc_bin_is_exactly_the_sum_of_the_input() {
        let input: Vec<f64> = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let spectrum = real_fft(&input).unwrap();
        assert!(close(spectrum[0].re, 36.0, 1e-9));
        assert!(close(spectrum[0].im, 0.0, 1e-12));
    }

    /// Parseval's theorem for the real DFT: `Σ|x[n]|² = (1/N) Σ_k w_k
    /// |X[k]|²`, `w_k = 1` for the self-conjugate bins (`k=0`, and `k=N/2`
    /// when `N` is even) and `2` for every other bin — **not** a plain
    /// unweighted sum over the `N/2+1` returned bins, which double-counts
    /// nothing for the self-conjugate bins and under-counts everything
    /// else. Checked for both even and odd `N`.
    #[test]
    fn parseval_holds_for_the_real_dft() {
        for &n in &[8usize, 15, 16, 33] {
            let input: Vec<f64> = (0..n).map(|k| (k as f64 * 0.71).sin() + 0.4).collect();
            let spectrum = real_fft(&input).unwrap();

            let time_energy: f64 = input.iter().map(|x| x * x).sum();

            let nyquist_bin = if n % 2 == 0 { Some(n / 2) } else { None };
            let freq_energy: f64 = spectrum
                .iter()
                .enumerate()
                .map(|(k, bin)| {
                    let weight = if k == 0 || Some(k) == nyquist_bin {
                        1.0
                    } else {
                        2.0
                    };
                    weight * (bin.re * bin.re + bin.im * bin.im)
                })
                .sum::<f64>()
                / n as f64;

            assert!(
                close(time_energy, freq_energy, 1e-6),
                "n={n}: time={time_energy} vs freq={freq_energy}"
            );
        }
    }

    // ---- ComplexSpectrum ----

    #[test]
    fn from_complex_and_to_complex_round_trip() {
        let values = vec![Complex::new(1.0_f64, 2.0), Complex::new(-3.0, 4.5)];
        let spectrum = ComplexSpectrum::from_complex(&values);
        assert_eq!(spectrum.len(), 2);
        assert!(!spectrum.is_empty());
        let back = spectrum.to_complex();
        for (a, b) in values.iter().zip(back.iter()) {
            assert!(close(a.re, b.re, 1e-6) && close(a.im, b.im, 1e-6));
        }
    }

    #[test]
    fn magnitude_and_phase_match_hand_computed_values() {
        let spectrum = ComplexSpectrum {
            real: vec![3.0, 0.0],
            imag: vec![4.0, 0.0],
        };
        assert_eq!(spectrum.magnitude(), vec![5.0, 0.0]);
        assert!((spectrum.phase()[0] - (4.0_f32).atan2(3.0)).abs() < 1e-6);
    }

    #[test]
    fn real_and_imag_view_are_checked_tensors_over_the_flat_columns() {
        let spectrum = ComplexSpectrum {
            real: vec![1.0, 2.0, 3.0],
            imag: vec![4.0, 5.0, 6.0],
        };
        let real_view = spectrum.real_view().unwrap();
        assert_eq!(real_view.shape(), &[3]);
        assert_eq!(real_view.as_slice(), Some(&[1.0, 2.0, 3.0][..]));
        let imag_view = spectrum.imag_view().unwrap();
        assert_eq!(imag_view.get(&[1]).unwrap(), 5.0);
    }

    #[test]
    fn spectrum_round_trips_through_a_record_batch() {
        let spectrum = ComplexSpectrum {
            real: vec![1.0, 2.0],
            imag: vec![3.0, 4.0],
        };
        let batch = spectrum.to_record_batch().unwrap();
        assert_eq!(
            ComplexSpectrum::from_record_batch(&batch).unwrap(),
            spectrum
        );
        assert_eq!(
            <ComplexSpectrum as FromPayload>::from_batch(&batch).unwrap(),
            spectrum
        );
    }

    // ---- Operators ----

    fn input_event(payload: Vec<u8>) -> OpEvent {
        use astrs_time::HlcTimestamp;
        use astrs_wire::{DataId, Metadata};
        OpEvent::Input {
            id: DataId::new("in").unwrap(),
            source: "sensor/audio".parse().unwrap(),
            metadata: Metadata::new(HlcTimestamp::EPOCH),
            payload,
        }
    }

    fn encode<M: AstrsMessage>(message: &M) -> Vec<u8> {
        astrs_data::ipc::encode_payload(&message.to_record_batch().unwrap())
            .unwrap()
            .to_vec()
    }

    #[test]
    fn rfft_then_irfft_operators_round_trip_a_frame() {
        let mut rfft_op = RfftOperator::default();
        let mut irfft_op = IrfftOperator::default();
        let mut out = OpOutput::new();

        let original: Vec<f32> = (0..16).map(|n| (n as f32 * 0.4).sin()).collect();
        let event = input_event(encode(&Frame::new(original.clone())));
        rfft_op.on_event(&event, &mut out).unwrap();
        let spectrum_bytes = out.drain().remove(0).into_parts().2;

        let event2 = input_event(spectrum_bytes);
        irfft_op.on_event(&event2, &mut out).unwrap();
        let signal_bytes = out.drain().remove(0).into_parts().2;
        let decoded = astrs_data::ipc::decode_payload(&signal_bytes).unwrap();
        let result = Frame::from_record_batch(&decoded).unwrap();

        for (a, b) in original.iter().zip(result.samples.iter()) {
            assert!((a - b).abs() < 1e-4, "{a} vs {b}");
        }
    }

    #[test]
    fn fft_then_ifft_operators_round_trip_a_complex_vector() {
        let mut fft_op = FftOperator::default();
        let mut ifft_op = IfftOperator::default();
        let mut out = OpOutput::new();

        let original = ComplexSpectrum {
            real: (0..8).map(|n| (n as f32 * 0.3).sin()).collect(),
            imag: (0..8).map(|n| (n as f32 * 0.2).cos()).collect(),
        };
        let event = input_event(encode(&original));
        fft_op.on_event(&event, &mut out).unwrap();
        let spectrum_bytes = out.drain().remove(0).into_parts().2;

        let event2 = input_event(spectrum_bytes);
        ifft_op.on_event(&event2, &mut out).unwrap();
        let signal_bytes = out.drain().remove(0).into_parts().2;
        let decoded = astrs_data::ipc::decode_payload(&signal_bytes).unwrap();
        let result = ComplexSpectrum::from_record_batch(&decoded).unwrap();

        for i in 0..8 {
            assert!((original.real[i] - result.real[i]).abs() < 1e-4);
            assert!((original.imag[i] - result.imag[i]).abs() < 1e-4);
        }
    }

    #[test]
    fn irfft_operator_defaults_output_length_to_the_even_convention() {
        let mut op = IrfftOperator::default();
        let mut out = OpOutput::new();
        // 5 bins => default output_length = 2*(5-1) = 8.
        let spectrum = ComplexSpectrum {
            real: vec![0.0; 5],
            imag: vec![0.0; 5],
        };
        op.on_event(&input_event(encode(&spectrum)), &mut out)
            .unwrap();
        let bytes = out.drain().remove(0).into_parts().2;
        let decoded = astrs_data::ipc::decode_payload(&bytes).unwrap();
        let result = Frame::from_record_batch(&decoded).unwrap();
        assert_eq!(result.samples.len(), 8);
    }

    #[test]
    fn irfft_operator_honors_a_configured_output_length() {
        let mut op = IrfftOperator::default();
        let mut config = BTreeMap::new();
        config.insert("output_length".to_owned(), Parameter::Integer(9));
        op.configure(&config).unwrap();

        let mut out = OpOutput::new();
        let spectrum = ComplexSpectrum {
            real: vec![0.0; 5], // n/2+1 = 9/2+1 = 5, consistent
            imag: vec![0.0; 5],
        };
        op.on_event(&input_event(encode(&spectrum)), &mut out)
            .unwrap();
        let bytes = out.drain().remove(0).into_parts().2;
        let decoded = astrs_data::ipc::decode_payload(&bytes).unwrap();
        let result = Frame::from_record_batch(&decoded).unwrap();
        assert_eq!(result.samples.len(), 9);
    }

    #[test]
    fn plan_cache_is_reused_across_events_of_the_same_size_and_rebuilt_on_change() {
        let mut op = RfftOperator::default();
        let mut out = OpOutput::new();
        let frame16 = Frame::new(vec![0.0_f32; 16]);
        op.on_event(&input_event(encode(&frame16)), &mut out)
            .unwrap();
        assert_eq!(op.plan.as_ref().unwrap().size, 16);
        out.drain();

        // A different frame size must rebuild the cache, not panic on a
        // stale size mismatch.
        let frame32 = Frame::new(vec![0.0_f32; 32]);
        op.on_event(&input_event(encode(&frame32)), &mut out)
            .unwrap();
        assert_eq!(op.plan.as_ref().unwrap().size, 32);
    }

    #[test]
    fn empty_frame_is_a_typed_operator_error_not_a_panic() {
        let mut op = RfftOperator::default();
        let mut out = OpOutput::new();
        let err = op
            .on_event(
                &input_event(encode(&Frame::new(Vec::<f32>::new()))),
                &mut out,
            )
            .unwrap_err();
        assert!(matches!(err, OpError::Failed { .. }));
    }

    #[test]
    fn operator_entry_names_are_snake_case() {
        assert_eq!(FftOperator::operator_entry().0, "fft_operator");
        assert_eq!(IfftOperator::operator_entry().0, "ifft_operator");
        assert_eq!(RfftOperator::operator_entry().0, "rfft_operator");
        assert_eq!(IrfftOperator::operator_entry().0, "irfft_operator");
    }
}
