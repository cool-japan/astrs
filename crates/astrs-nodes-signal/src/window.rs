//! [`WindowFunction`] — tapers applied to a frame of samples before a
//! transform, plus [`WindowOperator`], the astrs operator wrapping it.
//!
//! # Windows are periodic, not symmetric
//!
//! [`WindowFunction::coefficient`] uses the DFT-even (periodic) definition —
//! `2πn/N`, not `2πn/(N-1)`. That is the correct choice for spectral
//! analysis, where the window is one period of a signal the transform treats
//! as circular; the symmetric form belongs to FIR filter design
//! (`crate::fir`), which is a different stage and uses
//! [`WindowFunction::coefficient_symmetric`] instead. The practical
//! consequence for the periodic form is that it reaches its peak exactly at
//! `n == N/2` and its first coefficient is the same as the one that *would*
//! follow the last:
//!
//! ```
//! use astrs_nodes_signal::WindowFunction;
//!
//! // Hann: zero at the frame edge, exactly 1.0 at the centre bin.
//! assert!(WindowFunction::Hann.coefficient(0, 8).abs() < 1e-12);
//! assert!((WindowFunction::Hann.coefficient(4, 8) - 1.0).abs() < 1e-12);
//!
//! // Rectangular is the identity window: applying it changes nothing.
//! let frame = [1.0_f64, 2.0, 3.0, 4.0];
//! let windowed: Vec<f64> = frame
//!     .iter()
//!     .enumerate()
//!     .map(|(n, sample)| sample * WindowFunction::Rectangular.coefficient(n, frame.len()))
//!     .collect();
//! assert_eq!(windowed, frame);
//! ```

use std::collections::BTreeMap;
use std::f64::consts::PI;

use astrs_operator_api::{OpError, OpEvent, OpOutput, OpResult, Operator, Status, operator};
use astrs_wire::Parameter;

use crate::message::Frame;
use crate::support::decode;

/// A window applied to a frame of samples before a transform.
///
/// The periodic ([`WindowFunction::coefficient`]) and symmetric
/// ([`WindowFunction::coefficient_symmetric`]) forms are both available on
/// every variant — see the module documentation for which stage
/// wants which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum WindowFunction {
    /// No taper at all: every coefficient is `1.0`. The default, because it
    /// is the only window that changes nothing about the input.
    #[default]
    Rectangular,
    /// A raised-cosine taper: `0.5 - 0.5·cos(2πn/N)`. The usual first choice
    /// for spectral analysis.
    Hann,
    /// `0.54 - 0.46·cos(2πn/N)`: slightly higher first sidelobe suppression
    /// than Hann at the cost of a non-zero value at the frame edge.
    Hamming,
    /// `0.42 - 0.5·cos(2πn/N) + 0.08·cos(4πn/N)`: a wider main lobe for
    /// considerably lower sidelobes than Hann.
    Blackman,
    /// The 4-term Blackman-Harris window (Harris, "On the Use of Windows for
    /// Harmonic Analysis with the Discrete Fourier Transform", 1978):
    /// `a0 - a1·cos(2πn/N) + a2·cos(4πn/N) - a3·cos(6πn/N)` with
    /// `a0=0.35875, a1=0.48829, a2=0.14128, a3=0.01168`. Minimum sidelobe
    /// level (~-92 dB) among the windows this crate offers, at the cost of
    /// the widest main lobe.
    BlackmanHarris,
}

/// The 4-term Blackman-Harris coefficients (Harris 1978), in order
/// `[a0, a1, a2, a3]`. They sum to exactly `1.0` by construction, which is
/// what makes the window's periodic peak (`n == N/2`) exactly unity.
const BLACKMAN_HARRIS: [f64; 4] = [0.358_75, 0.488_29, 0.141_28, 0.011_68];

impl WindowFunction {
    /// This window's coefficient for sample `n` of a frame of `size`
    /// samples, using the periodic (DFT-even) definition (see the module
    /// documentation).
    ///
    /// `n` is not required to be less than `size`; the periodic definition is
    /// well-defined for any index, and a caller iterating a frame will never
    /// exceed it anyway. A `size` of `0` yields `1.0` — there is no frame to
    /// taper, and returning `NaN` from a division by zero would silently
    /// poison every downstream sample instead.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_nodes_signal::WindowFunction;
    ///
    /// assert_eq!(WindowFunction::Rectangular.coefficient(7, 16), 1.0);
    /// // Hamming does not reach zero at the frame edge; Hann and Blackman do.
    /// assert!((WindowFunction::Hamming.coefficient(0, 16) - 0.08).abs() < 1e-12);
    /// ```
    #[must_use]
    pub fn coefficient(self, n: usize, size: usize) -> f64 {
        if self == Self::Rectangular || size == 0 {
            return 1.0;
        }
        let phase = 2.0 * PI * (n as f64) / (size as f64);
        Self::taper(self, phase)
    }

    /// This window's coefficient for sample `n` of a frame of `size` samples,
    /// using the **symmetric** definition (`2πn/(size-1)`) that FIR filter
    /// design (`crate::fir`) wants instead of the periodic one above — see
    /// the module documentation for why the two stages disagree.
    ///
    /// A symmetric window satisfies `coefficient_symmetric(0) ==
    /// coefficient_symmetric(size - 1)` exactly, which is what preserves a
    /// windowed-sinc FIR design's exact linear phase (a real, symmetric
    /// impulse response). `size <= 1` yields `1.0` for every `n` — there is
    /// no `size - 1` to divide by, and one or zero samples has nothing to be
    /// asymmetric about.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_nodes_signal::WindowFunction;
    ///
    /// // The periodic and symmetric forms of Hann agree at the *first*
    /// // sample (both start the cosine at phase 0)...
    /// assert_eq!(WindowFunction::Hann.coefficient(0, 9), 0.0);
    /// assert_eq!(WindowFunction::Hann.coefficient_symmetric(0, 9), 0.0);
    /// // ...but only the symmetric form also reaches zero at the *last*
    /// // sample, which is exactly the property FIR design needs.
    /// assert!((WindowFunction::Hann.coefficient_symmetric(8, 9)).abs() < 1e-12);
    /// assert!((WindowFunction::Hann.coefficient(8, 9) - WindowFunction::Hann.coefficient(0, 9)).abs() > 1e-3);
    /// ```
    #[must_use]
    pub fn coefficient_symmetric(self, n: usize, size: usize) -> f64 {
        if self == Self::Rectangular || size <= 1 {
            return 1.0;
        }
        let phase = 2.0 * PI * (n as f64) / ((size - 1) as f64);
        Self::taper(self, phase)
    }

    /// The shared raised-cosine-sum evaluation both
    /// [`WindowFunction::coefficient`] and
    /// [`WindowFunction::coefficient_symmetric`] apply to their own phase
    /// angle — the periodic/symmetric distinction lives entirely in how the
    /// caller derives `phase`, not in this formula.
    fn taper(self, phase: f64) -> f64 {
        match self {
            Self::Rectangular => 1.0,
            Self::Hann => 0.5 - 0.5 * phase.cos(),
            Self::Hamming => 0.54 - 0.46 * phase.cos(),
            Self::Blackman => 0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos(),
            Self::BlackmanHarris => {
                let [a0, a1, a2, a3] = BLACKMAN_HARRIS;
                a0 - a1 * phase.cos() + a2 * (2.0 * phase).cos() - a3 * (3.0 * phase).cos()
            }
        }
    }

    /// Every coefficient for a frame of `size` samples, in index order,
    /// using the periodic definition.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_nodes_signal::WindowFunction;
    ///
    /// let window = WindowFunction::Rectangular.coefficients(4);
    /// assert_eq!(window, vec![1.0; 4]);
    /// ```
    #[must_use]
    pub fn coefficients(self, size: usize) -> Vec<f64> {
        (0..size).map(|n| self.coefficient(n, size)).collect()
    }

    /// Every coefficient for a frame of `size` samples, in index order,
    /// using the symmetric definition FIR design wants.
    #[must_use]
    pub fn coefficients_symmetric(self, size: usize) -> Vec<f64> {
        (0..size)
            .map(|n| self.coefficient_symmetric(n, size))
            .collect()
    }

    /// This window's coherent gain: the mean of its (periodic) coefficients,
    /// and the factor a magnitude spectrum must be divided by to undo the
    /// amplitude the window itself removed.
    ///
    /// `0.0` for an empty frame, which has no coefficients to average.
    #[must_use]
    pub fn coherent_gain(self, size: usize) -> f64 {
        if size == 0 {
            return 0.0;
        }
        let sum: f64 = (0..size).map(|n| self.coefficient(n, size)).sum();
        sum / (size as f64)
    }

    /// Multiplies `samples` in place by this window's periodic coefficients.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_nodes_signal::WindowFunction;
    ///
    /// let mut frame = [1.0_f32, 1.0, 1.0, 1.0];
    /// WindowFunction::Hann.apply_f32(&mut frame);
    /// assert!(frame[0].abs() < 1e-6, "Hann vanishes at the frame edge");
    /// assert!((frame[2] - 1.0).abs() < 1e-6, "Hann peaks at the centre");
    /// ```
    pub fn apply_f32(self, samples: &mut [f32]) {
        let size = samples.len();
        for (n, sample) in samples.iter_mut().enumerate() {
            *sample *= self.coefficient(n, size) as f32;
        }
    }

    /// The `f64` counterpart of [`WindowFunction::apply_f32`].
    pub fn apply_f64(self, samples: &mut [f64]) {
        let size = samples.len();
        for (n, sample) in samples.iter_mut().enumerate() {
            *sample *= self.coefficient(n, size);
        }
    }

    /// Parses a manifest-facing name (`"rectangular"`, `"hann"`,
    /// `"hamming"`, `"blackman"`, `"blackman_harris"`) into a
    /// [`WindowFunction`], case-insensitively.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "rectangular" | "rect" | "none" => Some(Self::Rectangular),
            "hann" | "hanning" => Some(Self::Hann),
            "hamming" => Some(Self::Hamming),
            "blackman" => Some(Self::Blackman),
            "blackman_harris" | "blackman-harris" | "blackmanharris" => Some(Self::BlackmanHarris),
            _ => None,
        }
    }
}

/// Applies a [`WindowFunction`] to each incoming [`Frame`].
///
/// # Configuration
///
/// ```yaml
/// operators:
///   - id: taper
///     operator: WindowOperator
///     config:
///       window: "hann"   # rectangular | hann | hamming | blackman | blackman_harris
/// ```
///
/// `window` defaults to [`WindowFunction::Rectangular`] (a no-op) when
/// omitted, so an unconfigured `WindowOperator` is safe to wire in without
/// changing behaviour.
#[operator]
#[derive(Debug, Default)]
pub struct WindowOperator {
    window: WindowFunction,
}

impl Operator for WindowOperator {
    fn configure(&mut self, config: &BTreeMap<String, Parameter>) -> OpResult<()> {
        if let Some(Parameter::String(name)) = config.get("window") {
            self.window = WindowFunction::parse(name).ok_or_else(|| {
                OpError::failed(format!(
                    "unknown window {name:?}: expected one of rectangular, hann, hamming, \
                     blackman, blackman_harris"
                ))
            })?;
        }
        Ok(())
    }

    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input {
                metadata, payload, ..
            } => {
                let mut frame: Frame = decode(payload)?;
                self.window.apply_f32(&mut frame.samples);
                out.send("windowed", metadata.clone(), &frame)?;
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

    const EVERY_WINDOW: &[WindowFunction] = &[
        WindowFunction::Rectangular,
        WindowFunction::Hann,
        WindowFunction::Hamming,
        WindowFunction::Blackman,
        WindowFunction::BlackmanHarris,
    ];

    fn close(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-12
    }

    #[test]
    fn rectangular_is_the_identity_window_and_the_default() {
        assert_eq!(WindowFunction::default(), WindowFunction::Rectangular);
        for n in 0..16 {
            assert_eq!(WindowFunction::Rectangular.coefficient(n, 16), 1.0);
        }
    }

    #[test]
    fn the_periodic_definition_peaks_exactly_at_the_centre() {
        // The whole point of the DFT-even form: `n == N/2` is exactly 1.0
        // for Hann, which the symmetric `2πn/(N-1)` form never reaches.
        assert!(close(WindowFunction::Hann.coefficient(4, 8), 1.0));
        assert!(close(WindowFunction::Hann.coefficient(8, 16), 1.0));
    }

    #[test]
    fn hann_and_blackman_vanish_at_the_frame_edge_but_hamming_does_not() {
        assert!(close(WindowFunction::Hann.coefficient(0, 32), 0.0));
        assert!(close(WindowFunction::Blackman.coefficient(0, 32), 0.0));
        assert!(close(WindowFunction::Hamming.coefficient(0, 32), 0.08));
    }

    /// Blackman-Harris does **not** vanish at the frame edge — its `a0` term
    /// alone is `0.35875`, and the three cosine terms at phase 0 sum to
    /// `-0.48829 + 0.14128 - 0.01168 = -0.35869`, leaving a small residual
    /// (`6e-5`) rather than an exact zero. Its clean closed-form properties
    /// are instead: the four coefficients sum to exactly `1.0` (so the
    /// periodic peak at `n == N/2` is exact), and the coherent gain over a
    /// full period is exactly `a0`.
    #[test]
    fn blackman_harris_peaks_exactly_at_the_centre_but_not_at_the_edge() {
        assert!(close(
            WindowFunction::BlackmanHarris.coefficient(32, 64),
            1.0
        ));
        let edge = WindowFunction::BlackmanHarris.coefficient(0, 64);
        assert!(edge > 0.0 && edge < 1e-3, "edge value was {edge}");
    }

    #[test]
    fn an_empty_frame_never_produces_nan() {
        for window in EVERY_WINDOW {
            let value = window.coefficient(0, 0);
            assert!(value.is_finite(), "{window:?} produced {value}");
            assert_eq!(value, 1.0);
        }
    }

    #[test]
    fn coefficients_matches_coefficient_index_by_index() {
        for window in EVERY_WINDOW {
            let bulk = window.coefficients(12);
            assert_eq!(bulk.len(), 12);
            for (n, value) in bulk.iter().enumerate() {
                assert!(close(*value, window.coefficient(n, 12)), "{window:?} n={n}");
            }
        }
    }

    #[test]
    fn every_coefficient_is_finite_and_within_zero_to_one() {
        for window in EVERY_WINDOW {
            for value in window.coefficients(64) {
                assert!(value.is_finite(), "{window:?}");
                assert!(
                    (-1e-12..=1.0 + 1e-12).contains(&value),
                    "{window:?}: {value}"
                );
            }
        }
    }

    #[test]
    fn coherent_gain_is_one_for_rectangular_and_a_half_for_hann() {
        assert!(close(WindowFunction::Rectangular.coherent_gain(64), 1.0));
        assert!(close(WindowFunction::Hann.coherent_gain(64), 0.5));
        assert_eq!(WindowFunction::Hann.coherent_gain(0), 0.0);
    }

    /// The four Blackman-Harris coefficients sum to exactly `1.0` by
    /// construction (`0.35875 + 0.48829 + 0.14128 + 0.01168`), so its
    /// coherent gain over a full period equals `a0` exactly (the three
    /// cosine terms are each a discrete sum over a full period of a
    /// nonzero-frequency cosine, which vanishes exactly for any `size` that
    /// does not divide 1, 2 or 3 — true for every `size > 3`).
    #[test]
    fn blackman_harris_coherent_gain_is_its_a0_term() {
        assert!(close(
            WindowFunction::BlackmanHarris.coherent_gain(64),
            0.358_75
        ));
    }

    #[test]
    fn coefficient_symmetric_agrees_with_periodic_only_at_the_first_sample() {
        for window in EVERY_WINDOW {
            assert!(close(
                window.coefficient(0, 9),
                window.coefficient_symmetric(0, 9)
            ));
        }
        // But the *last* sample only matches under the symmetric form's own
        // defining property (equal to the first), not the periodic one.
        assert!(close(
            WindowFunction::Hann.coefficient_symmetric(8, 9),
            WindowFunction::Hann.coefficient_symmetric(0, 9)
        ));
        assert!(
            (WindowFunction::Hann.coefficient(8, 9) - WindowFunction::Hann.coefficient(0, 9)).abs()
                > 1e-3
        );
    }

    #[test]
    fn coefficient_symmetric_is_exactly_symmetric_for_every_window() {
        for window in EVERY_WINDOW {
            let taps = window.coefficients_symmetric(15);
            for i in 0..taps.len() {
                assert!(
                    close(taps[i], taps[taps.len() - 1 - i]),
                    "{window:?} tap {i} vs {}: {} vs {}",
                    taps.len() - 1 - i,
                    taps[i],
                    taps[taps.len() - 1 - i]
                );
            }
        }
    }

    #[test]
    fn coefficient_symmetric_of_a_single_or_empty_frame_is_one() {
        for window in EVERY_WINDOW {
            assert_eq!(window.coefficient_symmetric(0, 1), 1.0);
            assert_eq!(window.coefficient_symmetric(0, 0), 1.0);
        }
    }

    #[test]
    fn apply_f32_and_f64_multiply_in_place_by_the_periodic_coefficients() {
        let mut f32_frame = [2.0_f32, 2.0, 2.0, 2.0];
        WindowFunction::Rectangular.apply_f32(&mut f32_frame);
        assert_eq!(f32_frame, [2.0, 2.0, 2.0, 2.0]);

        let mut f64_frame = vec![1.0_f64; 8];
        WindowFunction::Hann.apply_f64(&mut f64_frame);
        for (n, sample) in f64_frame.iter().enumerate() {
            assert!(close(*sample, WindowFunction::Hann.coefficient(n, 8)));
        }
    }

    #[test]
    fn parse_recognizes_every_manifest_spelling_case_insensitively() {
        assert_eq!(
            WindowFunction::parse("RECT"),
            Some(WindowFunction::Rectangular)
        );
        assert_eq!(WindowFunction::parse("Hann"), Some(WindowFunction::Hann));
        assert_eq!(WindowFunction::parse("hanning"), Some(WindowFunction::Hann));
        assert_eq!(
            WindowFunction::parse("HAMMING"),
            Some(WindowFunction::Hamming)
        );
        assert_eq!(
            WindowFunction::parse("blackman"),
            Some(WindowFunction::Blackman)
        );
        assert_eq!(
            WindowFunction::parse("blackman-harris"),
            Some(WindowFunction::BlackmanHarris)
        );
        assert_eq!(
            WindowFunction::parse("blackmanharris"),
            Some(WindowFunction::BlackmanHarris)
        );
        assert_eq!(WindowFunction::parse("nonsense"), None);
    }

    #[test]
    fn window_operator_default_is_a_no_op_rectangular_pass_through() {
        use astrs_time::HlcTimestamp;
        use astrs_wire::{DataId, Metadata};

        let mut op = WindowOperator::default();
        let mut out = OpOutput::new();
        let frame = Frame::new(vec![1.0, 2.0, 3.0]);
        let event = OpEvent::Input {
            id: DataId::new("in").unwrap(),
            source: "sensor/audio".parse().unwrap(),
            metadata: Metadata::new(HlcTimestamp::EPOCH),
            payload: astrs_data::ipc::encode_payload(&frame.to_record_batch().unwrap())
                .unwrap()
                .to_vec(),
        };
        assert_eq!(op.on_event(&event, &mut out).unwrap(), Status::Continue);
        let sends = out.drain();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].id().as_str(), "windowed");
        let decoded = astrs_data::ipc::decode_payload(sends[0].payload()).unwrap();
        let result = Frame::from_record_batch(&decoded).unwrap();
        assert_eq!(result.samples, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn window_operator_configure_rejects_an_unknown_name() {
        let mut op = WindowOperator::default();
        let mut config = BTreeMap::new();
        config.insert(
            "window".to_owned(),
            Parameter::String("nonsense".to_owned()),
        );
        let err = op.configure(&config).unwrap_err();
        assert!(matches!(err, OpError::Failed { .. }));
    }

    #[test]
    fn window_operator_applies_the_configured_window() {
        use astrs_time::HlcTimestamp;
        use astrs_wire::{DataId, Metadata};

        let mut op = WindowOperator::default();
        let mut config = BTreeMap::new();
        config.insert("window".to_owned(), Parameter::String("hann".to_owned()));
        op.configure(&config).unwrap();

        let mut out = OpOutput::new();
        let frame = Frame::new(vec![1.0_f32; 8]);
        let event = OpEvent::Input {
            id: DataId::new("in").unwrap(),
            source: "sensor/audio".parse().unwrap(),
            metadata: Metadata::new(HlcTimestamp::EPOCH),
            payload: astrs_data::ipc::encode_payload(&frame.to_record_batch().unwrap())
                .unwrap()
                .to_vec(),
        };
        op.on_event(&event, &mut out).unwrap();
        let sends = out.drain();
        let decoded = astrs_data::ipc::decode_payload(sends[0].payload()).unwrap();
        let result = Frame::from_record_batch(&decoded).unwrap();
        assert!(result.samples[0].abs() < 1e-6);
        assert!((result.samples[4] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn operator_entry_name_is_snake_case() {
        let (name, _ctor) = WindowOperator::operator_entry();
        assert_eq!(name, "window_operator");
    }
}
