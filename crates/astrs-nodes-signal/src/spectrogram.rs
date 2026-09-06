//! Windowed short-time Fourier transform (STFT) magnitude spectrogram, and
//! [`SpectrogramOperator`].
//!
//! [`stft_magnitude`] slides a window of `frame_size` samples across the
//! signal in steps of `hop_size`, and for each position windows the frame
//! ([`WindowFunction`], periodic form — this is spectral analysis, exactly
//! the case `crate::window`'s own docs describe) and takes its real FFT
//! magnitude:
//!
//! ```text
//! frame_f[n] = signal[f*hop_size + n] * window(n, frame_size),   n = 0..frame_size
//! S[f][k]    = |rfft(frame_f)[k]|
//! ```
//!
//! [`Spectrogram`] then carries the result exactly the way
//! [`astrs_data::tensor::ImageView`] carries `std/media/v1/Image`: a flat
//! `Vec<f32>` plus the two dimensions needed to view it as a checked
//! [`astrs_data::tensor::TensorView`] — here `[frames, bins]` instead of
//! `[height, width, channels]`.

use std::collections::BTreeMap;

use astrs_data::tensor::TensorView;
use astrs_data::{RecordBatch, Result as DataResult};
use astrs_node_api::message::FromPayload;
use astrs_operator_api::{
    AstrsMessage, OpError, OpEvent, OpOutput, OpResult, Operator, Status, operator,
};
use astrs_wire::Parameter;

use crate::error::{SignalError, SignalResult};
use crate::fft::real_fft;
use crate::message::Frame;
use crate::support::decode;
use crate::window::WindowFunction;

/// How many frames [`stft_magnitude`] produces for a signal of `signal_len`
/// samples: `0` if the signal is shorter than one frame, otherwise the
/// number of times a `frame_size`-wide window can start at a multiple of
/// `hop_size` without running past the end.
#[must_use]
fn frame_count(signal_len: usize, frame_size: usize, hop_size: usize) -> usize {
    if signal_len < frame_size {
        0
    } else {
        (signal_len - frame_size) / hop_size + 1
    }
}

/// Computes a windowed STFT magnitude spectrogram. See the module
/// documentation for the exact formula.
///
/// Returns `frame_count` frames, each `frame_size / 2 + 1` magnitude bins
/// (the real FFT's non-redundant half), in the same row-major order
/// [`Spectrogram::from_frames`] flattens them into.
///
/// # Errors
///
/// [`SignalError::NoTaps`] if `frame_size == 0`, or
/// [`SignalError::ZeroFactor`] if `hop_size == 0`.
pub fn stft_magnitude(
    signal: &[f64],
    frame_size: usize,
    hop_size: usize,
    window: WindowFunction,
) -> SignalResult<Vec<Vec<f64>>> {
    if frame_size == 0 {
        return Err(SignalError::NoTaps { num_taps: 0 });
    }
    if hop_size == 0 {
        return Err(SignalError::ZeroFactor);
    }
    let frames = frame_count(signal.len(), frame_size, hop_size);
    let mut result = Vec::with_capacity(frames);
    for f in 0..frames {
        let start = f * hop_size;
        let mut windowed: Vec<f64> = signal[start..start + frame_size].to_vec();
        window.apply_f64(&mut windowed);
        let spectrum = real_fft(&windowed)?;
        result.push(spectrum.iter().map(|bin| bin.norm()).collect());
    }
    Ok(result)
}

/// A magnitude spectrogram: `frames` rows of `bins` columns, flattened
/// row-major — the same "flat data plus explicit shape" wire shape
/// [`astrs_data::tensor::ImageView`] uses for images.
///
/// ```
/// use astrs_nodes_signal::{Spectrogram, WindowFunction, stft_magnitude};
///
/// let signal: Vec<f64> = (0..64).map(|n| (n as f64 * 0.5).sin()).collect();
/// let frames = stft_magnitude(&signal, 16, 8, WindowFunction::Hann)?;
/// let spectrogram = Spectrogram::from_frames(&frames);
/// let view = spectrogram.view()?;
/// assert_eq!(view.shape(), &[spectrogram.frames as usize, spectrogram.bins as usize]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, AstrsMessage)]
#[astrs(urn = "std/signal/v1/Spectrogram")]
pub struct Spectrogram {
    /// Number of time frames (rows).
    pub frames: u32,
    /// Number of frequency bins per frame (columns).
    pub bins: u32,
    /// The magnitude values, row-major: `magnitude[f * bins + k]` is frame
    /// `f`'s bin `k`.
    pub magnitude: Vec<f32>,
}

impl Spectrogram {
    /// Builds a spectrogram from [`stft_magnitude`]'s own output, flattening
    /// it row-major. An empty `frames` slice yields a `0 x 0` spectrogram;
    /// otherwise every row is expected to share the first row's length
    /// (true of anything [`stft_magnitude`] itself produced) — a shorter
    /// row is zero-padded rather than panicking on the flattening index.
    #[must_use]
    pub fn from_frames(frames: &[Vec<f64>]) -> Self {
        let bins = frames.first().map_or(0, Vec::len);
        let mut magnitude = Vec::with_capacity(frames.len() * bins);
        for frame in frames {
            for k in 0..bins {
                magnitude.push(frame.get(k).copied().unwrap_or(0.0) as f32);
            }
        }
        Self {
            frames: frames.len() as u32,
            bins: bins as u32,
            magnitude,
        }
    }

    /// A checked, zero-copy `[frames, bins]` [`TensorView`] over
    /// [`Spectrogram::magnitude`].
    ///
    /// # Errors
    ///
    /// [`astrs_data::DataError::TensorShapeMismatch`] if `magnitude.len() !=
    /// frames * bins` (never the case for a spectrogram
    /// [`Spectrogram::from_frames`] itself built).
    pub fn view(&self) -> astrs_data::Result<TensorView<f32>> {
        TensorView::from_values(
            self.magnitude.clone(),
            [self.frames as usize, self.bins as usize],
        )
    }
}

impl FromPayload for Spectrogram {
    fn from_batch(batch: &RecordBatch) -> DataResult<Self> {
        <Self as AstrsMessage>::from_record_batch(batch)
    }
}

/// Computes a windowed STFT magnitude spectrogram from each incoming
/// [`Frame`].
///
/// # Configuration
///
/// ```yaml
/// operators:
///   - id: spectrogram
///     operator: SpectrogramOperator
///     config:
///       frame_size: 512
///       hop_size: 256       # optional, defaults to frame_size / 2
///       window: "hann"      # optional, defaults to hann
/// ```
#[operator]
#[derive(Debug, Default)]
pub struct SpectrogramOperator {
    frame_size: usize,
    hop_size: usize,
    window: WindowFunction,
    configured: bool,
}

impl Operator for SpectrogramOperator {
    fn configure(&mut self, config: &BTreeMap<String, Parameter>) -> OpResult<()> {
        let frame_size = match config.get("frame_size") {
            Some(Parameter::Integer(value)) if *value > 0 => *value as usize,
            Some(other) => {
                return Err(OpError::failed(format!(
                    "\"frame_size\" must be a positive integer, got {other:?}"
                )));
            }
            None => {
                return Err(OpError::failed(
                    "SpectrogramOperator requires \"frame_size\"",
                ));
            }
        };
        let hop_size = match config.get("hop_size") {
            Some(Parameter::Integer(value)) if *value > 0 => *value as usize,
            Some(other) => {
                return Err(OpError::failed(format!(
                    "\"hop_size\" must be a positive integer, got {other:?}"
                )));
            }
            None => (frame_size / 2).max(1),
        };
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
            .unwrap_or(WindowFunction::Hann);

        self.frame_size = frame_size;
        self.hop_size = hop_size;
        self.window = window;
        self.configured = true;
        Ok(())
    }

    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input {
                metadata, payload, ..
            } => {
                if !self.configured {
                    return Err(OpError::failed("SpectrogramOperator was never configured"));
                }
                let frame: Frame = decode(payload)?;
                let frames =
                    stft_magnitude(&frame.to_f64(), self.frame_size, self.hop_size, self.window)
                        .map_err(|err| OpError::failed(err.to_string()))?;
                out.send(
                    "spectrogram",
                    metadata.clone(),
                    &Spectrogram::from_frames(&frames),
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

    fn close(left: f64, right: f64, tol: f64) -> bool {
        (left - right).abs() < tol
    }

    #[test]
    fn frame_count_matches_a_hand_worked_example() {
        // 64 samples, 16-wide frames, hop 8: starts at 0,8,16,...,48 (49+16=65>64
        // stops before), i.e. (64-16)/8 + 1 = 7.
        assert_eq!(frame_count(64, 16, 8), 7);
        assert_eq!(frame_count(15, 16, 8), 0, "shorter than one frame");
        assert_eq!(frame_count(16, 16, 8), 1, "exactly one frame");
    }

    #[test]
    fn stft_rejects_zero_frame_or_hop_size() {
        assert_eq!(
            stft_magnitude(&[1.0; 10], 0, 4, WindowFunction::Hann).unwrap_err(),
            SignalError::NoTaps { num_taps: 0 }
        );
        assert_eq!(
            stft_magnitude(&[1.0; 10], 4, 0, WindowFunction::Hann).unwrap_err(),
            SignalError::ZeroFactor
        );
    }

    #[test]
    fn a_short_signal_produces_zero_frames_not_an_error() {
        let frames = stft_magnitude(&[1.0, 2.0, 3.0], 16, 8, WindowFunction::Hann).unwrap();
        assert!(frames.is_empty());
    }

    #[test]
    fn a_single_frame_matches_windowing_and_real_fft_directly() {
        let signal: Vec<f64> = (0..32).map(|n| (n as f64 * 0.4).sin()).collect();
        let frames = stft_magnitude(&signal, 32, 32, WindowFunction::Hann).unwrap();
        assert_eq!(frames.len(), 1);

        let mut windowed = signal.clone();
        WindowFunction::Hann.apply_f64(&mut windowed);
        let expected: Vec<f64> = real_fft(&windowed)
            .unwrap()
            .iter()
            .map(|bin| bin.norm())
            .collect();
        for (a, b) in frames[0].iter().zip(expected.iter()) {
            assert!(close(*a, *b, 1e-9));
        }
    }

    /// A pure tone at exactly bin `k0` shows its peak magnitude at bin `k0`
    /// in *every* frame — a closed-form spectral-content check independent
    /// of which window is applied (a window redistributes energy into
    /// neighbouring bins but never moves the peak of an on-bin tone).
    #[test]
    fn a_pure_tone_peaks_at_its_own_bin_in_every_frame() {
        let frame_size = 64;
        let k0 = 5; // bin index
        let signal: Vec<f64> = (0..256)
            .map(|n| (2.0 * std::f64::consts::PI * k0 as f64 * n as f64 / frame_size as f64).sin())
            .collect();
        let frames = stft_magnitude(&signal, frame_size, 32, WindowFunction::Hann).unwrap();
        assert!(!frames.is_empty());
        for frame in &frames {
            let (peak_bin, _) = frame
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .unwrap();
            assert_eq!(
                peak_bin, k0,
                "expected the peak at bin {k0}, frame was {frame:?}"
            );
        }
    }

    #[test]
    fn from_frames_flattens_row_major_and_view_recovers_each_cell() {
        let frames = vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]];
        let spectrogram = Spectrogram::from_frames(&frames);
        assert_eq!(spectrogram.frames, 2);
        assert_eq!(spectrogram.bins, 3);
        assert_eq!(spectrogram.magnitude, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);

        let view = spectrogram.view().unwrap();
        assert_eq!(view.shape(), &[2, 3]);
        assert_eq!(view.get(&[0, 0]).unwrap(), 1.0);
        assert_eq!(view.get(&[1, 2]).unwrap(), 6.0);
    }

    #[test]
    fn from_frames_of_an_empty_slice_is_a_zero_by_zero_spectrogram() {
        let spectrogram = Spectrogram::from_frames(&[]);
        assert_eq!(spectrogram.frames, 0);
        assert_eq!(spectrogram.bins, 0);
        assert!(spectrogram.magnitude.is_empty());
    }

    #[test]
    fn spectrogram_round_trips_through_a_record_batch() {
        let spectrogram = Spectrogram::from_frames(&[vec![1.0, 2.0], vec![3.0, 4.0]]);
        let batch = spectrogram.to_record_batch().unwrap();
        assert_eq!(Spectrogram::from_record_batch(&batch).unwrap(), spectrogram);
        assert_eq!(
            <Spectrogram as FromPayload>::from_batch(&batch).unwrap(),
            spectrogram
        );
    }

    // ---- SpectrogramOperator ----

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
    fn operator_configure_and_run() {
        let mut op = SpectrogramOperator::default();
        let mut config = BTreeMap::new();
        config.insert("frame_size".to_owned(), Parameter::Integer(16));
        op.configure(&config).unwrap();
        assert_eq!(op.hop_size, 8, "hop_size defaults to frame_size / 2");
        assert_eq!(op.window, WindowFunction::Hann);

        let mut out = OpOutput::new();
        let signal = Frame::from_f64(&(0..64).map(|n| (n as f64 * 0.3).sin()).collect::<Vec<_>>());
        op.on_event(&frame_event(&signal), &mut out).unwrap();
        let sends = out.drain();
        assert_eq!(sends[0].id().as_str(), "spectrogram");
        let decoded = astrs_data::ipc::decode_payload(sends[0].payload()).unwrap();
        let result = Spectrogram::from_record_batch(&decoded).unwrap();
        assert_eq!(result.bins, 9); // 16/2+1
        assert_eq!(result.frames, frame_count(64, 16, 8) as u32);
    }

    #[test]
    fn operator_rejects_input_before_configuration() {
        let mut op = SpectrogramOperator::default();
        let mut out = OpOutput::new();
        let err = op
            .on_event(&frame_event(&Frame::new(vec![0.0_f32; 4])), &mut out)
            .unwrap_err();
        assert!(matches!(err, OpError::Failed { .. }));
    }

    #[test]
    fn operator_entry_name_is_snake_case() {
        let (name, _ctor) = SpectrogramOperator::operator_entry();
        assert_eq!(name, "spectrogram_operator");
    }
}
