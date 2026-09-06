//! Ready-made AstRS signal-processing nodes and operators.
//!
//! A dataflow that has to spectrum-analyse an IMU stream, band-pass a
//! microphone or resample a lidar scan should not need a bespoke node written
//! from scratch. This crate ships the common stages twice over: as a plain,
//! directly callable Rust function or type (numerically tested against
//! closed-form cases), and as an [`astrs_operator_api::Operator`] —
//! configured through a manifest's `operators: config:` map
//! ([`astrs_wire::Parameter`]) and reading/writing [`astrs_data`]'s
//! columnar payloads — the same processing, driven either way.
//!
//! # Scope
//!
//! | Stage | Plain function/type | Operator |
//! |---|---|---|
//! | Complex FFT/IFFT | [`fft_forward`], [`fft_inverse`] | [`FftOperator`], [`IfftOperator`] |
//! | Real FFT/IFFT | [`real_fft`], [`real_ifft`] | [`RfftOperator`], [`IrfftOperator`] |
//! | Windows | [`WindowFunction`] | [`WindowOperator`] |
//! | RBJ biquad filters (lowpass/highpass/bandpass/notch) | [`RbjCoefficients`], [`Biquad`] | [`BiquadOperator`] |
//! | Windowed-sinc FIR design + streaming convolution | [`design_lowpass`] and siblings, [`FirFilter`] | [`FirOperator`] |
//! | Polyphase decimation/interpolation | [`Decimator`], [`Interpolator`] | [`DecimateOperator`], [`InterpolateOperator`] |
//! | Windowed STFT spectrogram | [`stft_magnitude`], [`Spectrogram`] | [`SpectrogramOperator`] |
//! | Moving / exponential average | [`MovingAverage`], [`ExponentialAverage`] | [`MovingAverageOperator`], [`ExponentialAverageOperator`] |
//!
//! # Pure Rust, including the transforms
//!
//! `oxifft` is the COOLJAPAN pure-Rust FFT (blueprint §18.1's replacement for
//! both FFTW and `rustfft`, the latter banned outright in `deny.toml`), so a
//! spectrum stage adds no C to the dependency graph and no `-sys` crate to
//! the `*-sys` sweep.
//!
//! # Compute wide, narrow at the wire
//!
//! Every stage here computes internally in `f64` (`f32` narrowing happens
//! only where a value crosses the [`Frame`]/[`ComplexSpectrum`]/[`Spectrogram`]
//! wire boundary) — a deliberate accuracy choice, not an unfinished `f32`
//! path; see [`Biquad`]'s own docs for why an IIR section's feedback state in
//! particular benefits from the wider type regardless of which precision the
//! caller asks for. [`fft_forward`]/[`fft_inverse`]/[`real_fft`]/[`real_ifft`]
//! are the one exception, staying generic over `oxifft::Float` (`f32` or
//! `f64`) since they carry no state across calls for rounding to compound in.
//!
//! # Windows are periodic, not symmetric
//!
//! [`WindowFunction`] uses the DFT-even (periodic) definition for spectral
//! analysis, and a separate symmetric form for FIR design — see that type's
//! own module docs (`crate::window`) for why the two stages need different
//! definitions of the same window.
//!
//! # Quick tour
//!
//! ```
//! use astrs_nodes_signal::{Biquad, RbjCoefficients};
//!
//! // Design a 1 kHz lowpass biquad at 48 kHz and run an impulse through it.
//! let coefficients = RbjCoefficients::lowpass(48_000.0, 1_000.0, std::f64::consts::FRAC_1_SQRT_2)?;
//! assert!((coefficients.magnitude_at(48_000.0, 1e-9) - 1.0).abs() < 1e-9); // unity gain at DC
//!
//! let mut filter = Biquad::new(coefficients);
//! let mut impulse = vec![0.0; 8];
//! impulse[0] = 1.0;
//! let response = filter.process_block_f64(&impulse);
//! assert_eq!(response.len(), 8);
//! # Ok::<(), astrs_nodes_signal::SignalError>(())
//! ```

mod average;
mod biquad;
mod error;
mod fft;
mod fir;
mod message;
mod resample;
mod spectrogram;
mod support;
mod window;

pub use crate::average::{
    ExponentialAverage, ExponentialAverageOperator, MovingAverage, MovingAverageOperator,
};
pub use crate::biquad::{Biquad, BiquadKind, BiquadOperator, RbjCoefficients};
pub use crate::error::{SignalError, SignalResult};
pub use crate::fft::{
    ComplexSpectrum, FftOperator, IfftOperator, IrfftOperator, RfftOperator, fft_forward,
    fft_inverse, real_fft, real_ifft,
};
pub use crate::fir::{
    FirFilter, FirOperator, design_bandpass, design_bandstop, design_highpass, design_lowpass,
};
pub use crate::message::Frame;
pub use crate::resample::{DecimateOperator, Decimator, InterpolateOperator, Interpolator};
pub use crate::spectrogram::{Spectrogram, SpectrogramOperator, stft_magnitude};
pub use crate::window::{WindowFunction, WindowOperator};

pub use oxifft::{Complex, Float};
