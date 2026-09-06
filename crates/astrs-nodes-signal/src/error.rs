//! [`SignalError`] — everything a DSP design or streaming routine in this
//! crate can reject.
//!
//! Kept separate from [`astrs_operator_api::OpError`]: a filter design
//! rejecting a bad cutoff frequency is a domain error with its own typed
//! shape, not an operator-hosting failure. Every `Operator` in this crate
//! converts a [`SignalError`] into [`astrs_operator_api::OpError::failed`] at
//! its own boundary (the same pattern `astrs-operator-api`'s own
//! `Thresholded` example uses for a rejected `configure()` value) rather than
//! this crate reaching upward with a `From` impl on a foreign type.

use thiserror::Error;

/// The result type every fallible plain function in this crate returns.
pub type SignalResult<T> = Result<T, SignalError>;

/// A rejected signal-processing parameter or an out-of-range runtime input.
///
/// `#[non_exhaustive]`: new variants may join as this crate's coverage grows.
#[derive(Debug, Clone, Copy, PartialEq, Error)]
#[non_exhaustive]
pub enum SignalError {
    /// A transform, filter or resampler was asked to operate on zero
    /// samples, which has no well-defined frequency content.
    #[error("{what} requires at least one sample, got 0")]
    EmptyInput {
        /// What operation rejected the empty input, e.g. `"rfft"`.
        what: &'static str,
    },

    /// A cutoff or center frequency was not strictly between DC and the
    /// Nyquist frequency (`sample_rate_hz / 2`).
    #[error(
        "cutoff frequency {cutoff_hz} Hz must satisfy 0 < cutoff < {nyquist_hz} Hz \
         (Nyquist = sample_rate / 2)"
    )]
    CutoffOutOfRange {
        /// The rejected cutoff frequency, in Hz.
        cutoff_hz: f64,
        /// The sample rate's Nyquist frequency, in Hz.
        nyquist_hz: f64,
    },

    /// A band-pass/band-stop design's edges were not a valid, ordered,
    /// sub-Nyquist band.
    #[error(
        "band edges must satisfy 0 < low_hz ({low_hz}) < high_hz ({high_hz}) < \
         {nyquist_hz} Hz (Nyquist = sample_rate / 2)"
    )]
    InvalidBand {
        /// The rejected lower edge, in Hz.
        low_hz: f64,
        /// The rejected upper edge, in Hz.
        high_hz: f64,
        /// The sample rate's Nyquist frequency, in Hz.
        nyquist_hz: f64,
    },

    /// An RBJ biquad's quality factor was not strictly positive.
    #[error("quality factor Q must be positive and finite, got {q}")]
    NonPositiveQ {
        /// The rejected Q value.
        q: f64,
    },

    /// A sample rate was not strictly positive and finite.
    #[error("sample rate must be positive and finite, got {sample_rate_hz} Hz")]
    InvalidSampleRate {
        /// The rejected sample rate, in Hz.
        sample_rate_hz: f64,
    },

    /// A windowed-sinc FIR design was asked for a tap count that leaves no
    /// filter at all, or (for `highpass`/`bandstop`, which spectrally invert
    /// a lowpass prototype around one exact center tap) an even count with
    /// no single center index.
    #[error(
        "FIR spectral inversion (highpass/bandstop) needs an odd tap count for an exact \
         center tap, got {num_taps}"
    )]
    EvenTapsForSpectralInversion {
        /// The rejected, even tap count.
        num_taps: usize,
    },

    /// A FIR design was asked for fewer than one tap.
    #[error("FIR design needs at least 1 tap, got {num_taps}")]
    NoTaps {
        /// The rejected tap count.
        num_taps: usize,
    },

    /// A resampling ratio (decimation or interpolation factor) was zero.
    #[error("resampling factor must be at least 1, got 0")]
    ZeroFactor,

    /// An exponential average's smoothing coefficient was outside `(0, 1]`.
    #[error("exponential average alpha must satisfy 0 < alpha <= 1, got {alpha}")]
    InvalidAlpha {
        /// The rejected alpha.
        alpha: f64,
    },

    /// A moving average's window capacity was zero.
    #[error("moving average capacity must be at least 1, got 0")]
    ZeroCapacity,

    /// A transform size that `oxifft` cannot build a plan for under
    /// `Flags::WISDOM_ONLY` planning (the only planning mode that can
    /// legitimately fail — see `crate::fft`'s module docs for why the
    /// `Flags::ESTIMATE` this crate otherwise uses never hits this path in
    /// practice, and why every wrapper in this crate reports it as a typed
    /// error instead of assuming that guarantee).
    #[error("oxifft could not build a transform plan for size {size}")]
    UnsupportedTransformSize {
        /// The rejected transform size.
        size: usize,
    },

    /// A complex-to-real inverse transform's declared output length could
    /// not have produced the given number of complex bins under the
    /// Hermitian-symmetric `n/2+1` convention.
    #[error("irfft output length {n} is inconsistent with {bins} input bins (expected {expected})")]
    InconsistentIrfftLength {
        /// The requested real output length.
        n: usize,
        /// The number of complex input bins actually given.
        bins: usize,
        /// The bin count `n` would imply (`n / 2 + 1`).
        expected: usize,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_variant_renders_a_readable_message() {
        let cases: &[SignalError] = &[
            SignalError::EmptyInput { what: "rfft" },
            SignalError::CutoffOutOfRange {
                cutoff_hz: -1.0,
                nyquist_hz: 8000.0,
            },
            SignalError::InvalidBand {
                low_hz: 500.0,
                high_hz: 100.0,
                nyquist_hz: 8000.0,
            },
            SignalError::NonPositiveQ { q: -0.5 },
            SignalError::InvalidSampleRate {
                sample_rate_hz: 0.0,
            },
            SignalError::EvenTapsForSpectralInversion { num_taps: 10 },
            SignalError::NoTaps { num_taps: 0 },
            SignalError::ZeroFactor,
            SignalError::UnsupportedTransformSize { size: 0 },
            SignalError::InvalidAlpha { alpha: 1.5 },
            SignalError::ZeroCapacity,
            SignalError::InconsistentIrfftLength {
                n: 10,
                bins: 3,
                expected: 6,
            },
        ];
        for case in cases {
            let message = case.to_string();
            assert!(!message.is_empty(), "{case:?} rendered an empty message");
        }
    }

    #[test]
    fn signal_error_is_copy_and_compares_by_value() {
        let a = SignalError::ZeroFactor;
        let b = a;
        assert_eq!(a, b);
        assert_ne!(SignalError::ZeroFactor, SignalError::ZeroCapacity);
    }
}
