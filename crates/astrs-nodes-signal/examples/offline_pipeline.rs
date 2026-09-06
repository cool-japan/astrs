//! A full offline signal-processing pipeline built entirely from this
//! crate's **plain functions and types** — no [`astrs_operator_api::Operator`],
//! no manifest, no daemon. See `manifest_operators` (this crate's other
//! example) for the operator-wiring half of the same story.
//!
//! Every stage in `astrs_nodes_signal`'s own module table gets exercised
//! here, back to back, on synthetic data: an RBJ [`Biquad`], a windowed-sinc
//! [`FirFilter`], a [`WindowFunction`] into [`real_fft`] (wrapped as a typed
//! [`ComplexSpectrum`]), polyphase [`Decimator`]/[`Interpolator`],
//! [`stft_magnitude`] into a [`Spectrogram`], and both averages.
//!
//! Run it with:
//!
//! ```text
//! cargo run -p astrs-nodes-signal --example offline_pipeline
//! ```

use astrs_nodes_signal::{
    Biquad, ComplexSpectrum, Decimator, ExponentialAverage, FirFilter, Interpolator, MovingAverage,
    RbjCoefficients, Spectrogram, WindowFunction, design_bandpass, design_lowpass, real_fft,
    stft_magnitude,
};

/// Sample rate the audio-style stages (biquad, FFT, decimation,
/// interpolation, spectrogram) operate at, in Hz.
const SAMPLE_RATE_HZ: f64 = 8_000.0;

/// Length of the synthetic composite signal, in samples (a power of two,
/// chosen so [`SAMPLE_RATE_HZ`] / this length is an exact bin width and
/// `2048 / DECIMATION_FACTOR` divides evenly).
const NUM_SAMPLES: usize = 2_048;

/// Frequency of the tone this pipeline is meant to recover, in Hz.
const WANTED_HZ: f64 = 300.0;

/// Frequency of the tone the lowpass/decimation stages are meant to reject,
/// in Hz.
const UNWANTED_HZ: f64 = 2_800.0;

/// Cutoff of the RBJ lowpass biquad that separates [`WANTED_HZ`] from
/// [`UNWANTED_HZ`], in Hz.
const BIQUAD_CUTOFF_HZ: f64 = 900.0;

/// Integer factor the decimation/interpolation stage resamples by.
const DECIMATION_FACTOR: usize = 4;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let signal = synthesize_composite_tone();
    println!(
        "1. synthesized {} samples at {SAMPLE_RATE_HZ} Hz: {WANTED_HZ} Hz + {UNWANTED_HZ} Hz",
        signal.len()
    );

    let filtered = lowpass_filter(&signal)?;
    println!("2. RBJ lowpass biquad (cutoff {BIQUAD_CUTOFF_HZ} Hz) applied");

    demonstrate_fir_impulse_response()?;

    let peak_hz = find_spectral_peak(&filtered)?;
    println!("4. windowed real FFT peak at {peak_hz:.2} Hz (expected near {WANTED_HZ} Hz)");

    let (decimated_len, interpolated_len) = resample_round_trip(&signal)?;
    println!(
        "5. decimate x{DECIMATION_FACTOR} then interpolate x{DECIMATION_FACTOR}: \
         {} -> {decimated_len} -> {interpolated_len} samples",
        signal.len()
    );

    let spectrogram = analyze_spectrogram(&filtered)?;
    println!(
        "6. spectrogram: {} frame(s) x {} bin(s)",
        spectrogram.frames, spectrogram.bins
    );

    smooth_a_noisy_reading()?;
    println!("7. moving/exponential average smoothing complete (see peak-to-peak above)");

    println!("pipeline complete");
    Ok(())
}

/// Builds the synthetic test signal: [`WANTED_HZ`] plus a stronger, faster
/// [`UNWANTED_HZ`] tone, sampled at [`SAMPLE_RATE_HZ`] for [`NUM_SAMPLES`]
/// samples.
fn synthesize_composite_tone() -> Vec<f64> {
    (0..NUM_SAMPLES)
        .map(|n| {
            let t = n as f64 / SAMPLE_RATE_HZ;
            (2.0 * std::f64::consts::PI * WANTED_HZ * t).sin()
                + 0.6 * (2.0 * std::f64::consts::PI * UNWANTED_HZ * t).sin()
        })
        .collect()
}

/// Step 2: designs an RBJ lowpass [`Biquad`] at [`BIQUAD_CUTOFF_HZ`] and
/// streams `signal` through it in two half-sized calls, demonstrating that a
/// biquad's state genuinely carries across call boundaries (splitting the
/// input changes nothing about the output).
fn lowpass_filter(signal: &[f64]) -> Result<Vec<f64>, Box<dyn std::error::Error>> {
    let coefficients = RbjCoefficients::lowpass(
        SAMPLE_RATE_HZ,
        BIQUAD_CUTOFF_HZ,
        std::f64::consts::FRAC_1_SQRT_2,
    )?;
    let mut filter = Biquad::new(coefficients);
    let midpoint = signal.len() / 2;
    let mut filtered = filter.process_block_f64(&signal[..midpoint]);
    filtered.extend(filter.process_block_f64(&signal[midpoint..]));
    Ok(filtered)
}

/// Step 3: designs a windowed-sinc bandpass [`FirFilter`] around
/// [`WANTED_HZ`] and confirms the textbook identity that an FIR filter's
/// impulse response **is** its own taps, exactly — the same closed-form
/// check this crate's own unit tests use, run here against a fresh filter to
/// double as a live demonstration of streaming convolution.
fn demonstrate_fir_impulse_response() -> Result<(), Box<dyn std::error::Error>> {
    let taps = design_bandpass(
        127,
        WANTED_HZ - 50.0,
        WANTED_HZ + 50.0,
        SAMPLE_RATE_HZ,
        WindowFunction::Hamming,
    )?;
    let mut filter = FirFilter::new(taps.clone())?;
    let mut impulse = vec![0.0_f64; taps.len() + 8];
    impulse[0] = 1.0;
    let response = filter.process(&impulse);
    let matches_taps = response[..taps.len()] == taps[..];
    let tail_is_silent = response[taps.len()..].iter().all(|&y| y == 0.0);
    println!(
        "3. windowed-sinc FIR bandpass ({} taps): impulse response equals the taps exactly: {matches_taps}, \
         tail after the taps is silent: {tail_is_silent}",
        taps.len()
    );
    Ok(())
}

/// Step 4: windows `filtered` (periodic Hann, the spectral-analysis form —
/// see [`WindowFunction`]'s own module docs), takes its real FFT, wraps the
/// result as a typed [`ComplexSpectrum`] (touching its
/// [`ComplexSpectrum::real_view`] tensor accessor along the way), and
/// returns the frequency, in Hz, of the bin with the largest magnitude.
fn find_spectral_peak(filtered: &[f64]) -> Result<f64, Box<dyn std::error::Error>> {
    let mut windowed = filtered.to_vec();
    WindowFunction::Hann.apply_f64(&mut windowed);
    let spectrum = real_fft(&windowed)?;
    let spectrum = ComplexSpectrum::from_complex(&spectrum);

    // Exercise the checked, zero-copy tensor accessor `fft`'s own module
    // docs describe — not strictly needed to find the peak below, but the
    // point this step exists to make.
    let real_view = spectrum.real_view()?;
    println!(
        "   ComplexSpectrum::real_view shape: {:?}",
        real_view.shape()
    );

    let magnitude = spectrum.magnitude();
    let Some((peak_bin, _peak_magnitude)) = magnitude
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
    else {
        return Err("empty magnitude spectrum".into());
    };
    let bin_width_hz = SAMPLE_RATE_HZ / windowed.len() as f64;
    let peak_hz = peak_bin as f64 * bin_width_hz;

    let expected_bin = (WANTED_HZ * windowed.len() as f64 / SAMPLE_RATE_HZ).round() as usize;
    if peak_bin.abs_diff(expected_bin) > 1 {
        return Err(format!(
            "peak bin {peak_bin} (={peak_hz:.2} Hz) strayed more than one bin from the \
             expected bin {expected_bin} (~{WANTED_HZ} Hz)"
        )
        .into());
    }
    Ok(peak_hz)
}

/// Step 5: decimates `signal` by [`DECIMATION_FACTOR`] (with a lowpass
/// anti-aliasing filter designed at the new Nyquist frequency, exactly as
/// [`astrs_nodes_signal::DecimateOperator`]'s own `configure` does), then
/// interpolates the result back up by the same factor (with a
/// reconstruction filter designed exactly as
/// [`astrs_nodes_signal::InterpolateOperator::configure`] does), returning
/// `(decimated_len, interpolated_len)`. Since [`NUM_SAMPLES`] is an exact
/// multiple of [`DECIMATION_FACTOR`], both stages round-trip the sample
/// count exactly: `2048 -> 512 -> 2048`.
fn resample_round_trip(signal: &[f64]) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let decimated_rate_hz = SAMPLE_RATE_HZ / DECIMATION_FACTOR as f64;

    let anti_alias_taps = design_lowpass(
        31,
        decimated_rate_hz / 2.0,
        SAMPLE_RATE_HZ,
        WindowFunction::Hamming,
    )?;
    let mut decimator = Decimator::new(DECIMATION_FACTOR, &anti_alias_taps)?;
    let decimated = decimator.process(signal);

    let reconstruction_taps = design_lowpass(
        31,
        decimated_rate_hz / 2.0,
        decimated_rate_hz * DECIMATION_FACTOR as f64,
        WindowFunction::Hamming,
    )?;
    let mut interpolator = Interpolator::new(DECIMATION_FACTOR, &reconstruction_taps)?;
    let interpolated = interpolator.process(&decimated);

    if interpolated.len() != signal.len() {
        return Err(format!(
            "round trip did not recover the original length: {} in, {} out",
            signal.len(),
            interpolated.len()
        )
        .into());
    }
    Ok((decimated.len(), interpolated.len()))
}

/// Step 6: computes a windowed STFT magnitude [`Spectrogram`] of `filtered`
/// and touches [`Spectrogram::view`]'s tensor accessor, matching
/// `SpectrogramOperator`'s own default `hop_size` (`frame_size / 2`) and
/// default `window` (Hann) by omitting both.
fn analyze_spectrogram(filtered: &[f64]) -> Result<Spectrogram, Box<dyn std::error::Error>> {
    const FRAME_SIZE: usize = 256;
    let hop_size = FRAME_SIZE / 2;
    let frames = stft_magnitude(filtered, FRAME_SIZE, hop_size, WindowFunction::Hann)?;
    let spectrogram = Spectrogram::from_frames(&frames);
    let view = spectrogram.view()?;
    println!("   Spectrogram::view shape: {:?}", view.shape());
    Ok(spectrogram)
}

/// Step 7: pushes a constant "true value" plus a fast, deterministic ripple
/// through both [`MovingAverage`] and [`ExponentialAverage`], printing each
/// one's tail peak-to-peak spread next to the raw signal's — smoothing a
/// persistent oscillation never removes it entirely, but a working smoother
/// visibly narrows its spread.
fn smooth_a_noisy_reading() -> Result<(), Box<dyn std::error::Error>> {
    const TRUE_VALUE: f64 = 5.0;
    const TELEMETRY_HZ: f64 = 100.0;
    const NUM_READINGS: usize = 60;
    const TAIL_START: usize = 30;

    let raw: Vec<f64> = (0..NUM_READINGS)
        .map(|n| TRUE_VALUE + 0.4 * (n as f64 * 1.3).sin())
        .collect();

    let mut moving = MovingAverage::new(6)?;
    let moving_averaged = moving.process(&raw);

    let mut exponential = ExponentialAverage::from_time_constant(TELEMETRY_HZ, 0.15)?;
    let exponentially_averaged = exponential.process(&raw);

    println!(
        "   tail peak-to-peak: raw {:.3}, moving-average {:.3}, exponential-average {:.3}",
        peak_to_peak(&raw[TAIL_START..]),
        peak_to_peak(&moving_averaged[TAIL_START..]),
        peak_to_peak(&exponentially_averaged[TAIL_START..]),
    );
    Ok(())
}

/// `max - min` over `values`, or `0.0` for an empty slice.
fn peak_to_peak(values: &[f64]) -> f64 {
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    if max.is_finite() && min.is_finite() {
        max - min
    } else {
        0.0
    }
}
