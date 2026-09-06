//! Binary thresholding: a fixed caller-supplied level, or one Otsu's method
//! (Otsu, 1979) picks automatically from the frame's own histogram.
//!
//! # The comparison is strict `>`
//!
//! [`threshold_fixed`] classifies a sample as foreground when it is
//! *strictly greater than* `level`, matching OpenCV's `THRESH_BINARY`
//! convention (`dst = maxval if src > thresh else 0`) rather than `>=` —
//! the two differ only for samples exactly equal to `level`, but a golden
//! test needs one fixed answer, so this module commits to `>` and tests it.
//! [`ThresholdMode::Invert`] swaps the two output values rather than the
//! comparison itself (`dst = 0 if src > thresh else maxval`).
//!
//! # Otsu on a histogram with one (or zero) populated bins
//!
//! [`otsu_level`] returns `0` for a frame with no separable classes — every
//! sample the same grey level, or no samples at all (a `0`x`0` frame). There
//! is no meaningful split to find in either case, and `0` is a
//! deterministic, argument-free answer rather than an arbitrary one: with
//! the `>` convention above, thresholding a uniform frame at level `0`
//! paints it uniformly foreground (every sample is `> 0`) unless the
//! frame's one grey level is itself `0`, which paints it uniformly
//! background — both defensible, and neither requires [`otsu_level`] to
//! invent a class boundary that was never there.

use crate::buffer::ImageBuffer;
use crate::error::Result;
use crate::pixel::PixelFormat;

/// The value [`threshold_fixed`] writes for a foreground sample (and, under
/// [`ThresholdMode::Invert`], the value it writes for background instead).
pub const FOREGROUND: u8 = 255;

/// The value [`threshold_fixed`] writes for a background sample.
pub const BACKGROUND: u8 = 0;

/// Whether [`threshold_fixed`]'s two output values are swapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThresholdMode {
    /// `dst = 255 if src > level else 0` — the default.
    #[default]
    Normal,
    /// `dst = 0 if src > level else 255`.
    Invert,
}

/// The 256-bin histogram of a [`PixelFormat::Mono8`] frame's samples.
fn histogram(image: &ImageBuffer) -> [u32; 256] {
    let mut counts = [0u32; 256];
    for &sample in image.data() {
        counts[sample as usize] += 1;
    }
    counts
}

/// Thresholds a [`PixelFormat::Mono8`] frame at a fixed `level` (see this
/// module's doc for the exact `>` comparison and [`ThresholdMode`]).
///
/// # Errors
///
/// [`crate::VisionError::UnsupportedFormat`] when `image.format()` is not
/// [`PixelFormat::Mono8`].
pub fn threshold_fixed(image: &ImageBuffer, level: u8, mode: ThresholdMode) -> Result<ImageBuffer> {
    image.require_format(PixelFormat::Mono8, "threshold::threshold_fixed")?;
    let (above, at_or_below) = match mode {
        ThresholdMode::Normal => (FOREGROUND, BACKGROUND),
        ThresholdMode::Invert => (BACKGROUND, FOREGROUND),
    };
    let out: Vec<u8> = image
        .data()
        .iter()
        .map(|&sample| if sample > level { above } else { at_or_below })
        .collect();
    ImageBuffer::new(PixelFormat::Mono8, image.width(), image.height(), out)
}

/// Picks the threshold level that maximises Otsu's between-class variance
/// over `image`'s own 256-bin histogram (see this module's doc for the
/// no-separable-classes case).
///
/// A tie (more than one level achieving the same maximal variance — the
/// common case for a frame with only two populated grey levels, where every
/// level strictly between them ties) is broken toward the **smallest**
/// level: this loop only replaces the running best on a strict `>`.
///
/// # Errors
///
/// [`crate::VisionError::UnsupportedFormat`] when `image.format()` is not
/// [`PixelFormat::Mono8`].
pub fn otsu_level(image: &ImageBuffer) -> Result<u8> {
    image.require_format(PixelFormat::Mono8, "threshold::otsu_level")?;
    let hist = histogram(image);
    let total = f64::from(image.data().len() as u32);

    let sum_all: f64 = hist
        .iter()
        .enumerate()
        .map(|(level, &count)| (level as f64) * f64::from(count))
        .sum();

    let mut weight_bg = 0.0f64;
    let mut sum_bg = 0.0f64;
    let mut best_level = 0u8;
    let mut best_variance = -1.0f64;

    for (level, &count) in hist.iter().enumerate() {
        weight_bg += f64::from(count);
        if weight_bg <= 0.0 {
            continue;
        }
        let weight_fg = total - weight_bg;
        if weight_fg <= 0.0 {
            break;
        }
        sum_bg += (level as f64) * f64::from(count);
        let mean_bg = sum_bg / weight_bg;
        let mean_fg = (sum_all - sum_bg) / weight_fg;
        let mean_delta = mean_bg - mean_fg;
        let variance = weight_bg * weight_fg * mean_delta * mean_delta;
        if variance > best_variance {
            best_variance = variance;
            // `level` ranges over `0..256`; only `0..=255` is ever reached
            // (`level == 256` would need `weight_fg` already `0`, which
            // broke the loop one iteration earlier).
            best_level = level as u8;
        }
    }
    Ok(best_level)
}

/// [`otsu_level`] followed by [`threshold_fixed`] at the level it chose.
///
/// # Errors
///
/// As [`otsu_level`].
pub fn threshold_otsu(image: &ImageBuffer, mode: ThresholdMode) -> Result<(ImageBuffer, u8)> {
    let level = otsu_level(image)?;
    let thresholded = threshold_fixed(image, level, mode)?;
    Ok((thresholded, level))
}

/// An [`astrs_operator_api::Operator`] applying [`threshold_fixed`] or
/// [`threshold_otsu`] to every `image` input, publishing the binary result
/// on `thresholded`.
///
/// # Configuration
///
/// | Key | Type | Default | Meaning |
/// |---|---|---|---|
/// | `mode` | string | `"otsu"` | `"fixed"` or `"otsu"` |
/// | `level` | integer | `128` | The fixed level, `0..=255`; ignored under `"otsu"` |
/// | `invert` | bool | `false` | Selects [`ThresholdMode::Invert`] |
#[derive(Debug, Default)]
pub struct ThresholdOperator {
    use_otsu: bool,
    level: u8,
    mode: ThresholdMode,
}

impl astrs_operator_api::Operator for ThresholdOperator {
    fn configure(
        &mut self,
        config: &std::collections::BTreeMap<String, astrs_wire::Parameter>,
    ) -> astrs_operator_api::OpResult<()> {
        self.use_otsu = true;
        self.level = 128;
        match config.get("mode").and_then(astrs_wire::Parameter::as_str) {
            Some("fixed") => self.use_otsu = false,
            Some("otsu") | None => self.use_otsu = true,
            Some(other) => {
                return Err(astrs_operator_api::OpError::failed(format!(
                    "threshold mode must be \"fixed\" or \"otsu\", got {other:?}"
                )));
            }
        }
        if let Some(value) = config
            .get("level")
            .and_then(astrs_wire::Parameter::as_integer)
        {
            self.level = u8::try_from(value).map_err(|_| {
                astrs_operator_api::OpError::failed(format!(
                    "threshold level must fit in 0..=255, got {value}"
                ))
            })?;
        }
        if let Some(true) = config
            .get("invert")
            .and_then(astrs_wire::Parameter::as_bool)
        {
            self.mode = ThresholdMode::Invert;
        }
        Ok(())
    }

    fn on_event(
        &mut self,
        event: &astrs_operator_api::OpEvent,
        out: &mut astrs_operator_api::OpOutput,
    ) -> astrs_operator_api::OpResult<astrs_operator_api::Status> {
        crate::ops::forward_image(event, out, "thresholded", |image| {
            if self.use_otsu {
                Ok(threshold_otsu(image, self.mode)?.0)
            } else {
                threshold_fixed(image, self.level, self.mode)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::error::VisionError;

    fn mono8(data: &[u8]) -> ImageBuffer {
        let width = data.len() as u32;
        ImageBuffer::new(PixelFormat::Mono8, width, 1, data.to_vec()).unwrap()
    }

    #[test]
    fn threshold_fixed_uses_a_strict_greater_than_comparison() {
        let image = mono8(&[0, 100, 127, 128, 200, 255]);
        let out = threshold_fixed(&image, 127, ThresholdMode::Normal).unwrap();
        assert_eq!(out.data(), &[0, 0, 0, 255, 255, 255]);
    }

    #[test]
    fn invert_swaps_the_two_output_values_not_the_comparison() {
        let image = mono8(&[0, 100, 127, 128, 200, 255]);
        let out = threshold_fixed(&image, 127, ThresholdMode::Invert).unwrap();
        assert_eq!(out.data(), &[255, 255, 255, 0, 0, 0]);
    }

    #[test]
    fn threshold_refuses_a_non_mono8_frame() {
        let rgb = ImageBuffer::zeroed(PixelFormat::Rgb8, 2, 2).unwrap();
        assert!(matches!(
            threshold_fixed(&rgb, 10, ThresholdMode::Normal),
            Err(VisionError::UnsupportedFormat {
                op: "threshold::threshold_fixed",
                ..
            })
        ));
    }

    #[test]
    fn otsu_finds_the_boundary_between_two_clean_clusters() {
        let image = mono8(&[10, 10, 10, 10, 200, 200, 200, 200]);
        // Every level in 10..=199 ties on between-class variance; the
        // smallest-wins rule (see this module's doc) picks 10.
        assert_eq!(otsu_level(&image).unwrap(), 10);
        let (thresholded, level) = threshold_otsu(&image, ThresholdMode::Normal).unwrap();
        assert_eq!(level, 10);
        assert_eq!(thresholded.data(), &[0, 0, 0, 0, 255, 255, 255, 255]);
    }

    #[test]
    fn otsu_on_a_uniform_frame_returns_zero() {
        let image = mono8(&[42; 6]);
        assert_eq!(otsu_level(&image).unwrap(), 0);
    }

    #[test]
    fn otsu_on_an_empty_frame_returns_zero_without_dividing_by_zero() {
        let image = ImageBuffer::zeroed(PixelFormat::Mono8, 0, 0).unwrap();
        assert_eq!(otsu_level(&image).unwrap(), 0);
    }

    #[test]
    fn otsu_separates_a_more_realistic_bimodal_histogram() {
        // A wider low cluster (0..=49) and a wider high cluster (200..=255),
        // unevenly sized, to exercise the weighted-mean arithmetic rather
        // than just two point masses.
        let mut data = Vec::new();
        data.extend(std::iter::repeat_n(20u8, 30));
        data.extend(std::iter::repeat_n(40u8, 10));
        data.extend(std::iter::repeat_n(220u8, 50));
        let image = mono8(&data);
        let level = otsu_level(&image).unwrap();
        assert!(
            (40..220).contains(&level),
            "level {level} should separate the two clusters"
        );
    }

    // ---- Operators ----
    //
    // `ThresholdOperator` stands in for every `crate::ops::forward_image`
    // -based operator in this crate (Resize, Blur, Sobel, Threshold,
    // Morphology, ColorConvert and DrawRect all share that one `on_event`
    // body -- see `ops.rs`'s own module doc). Driving it through a real
    // `OpEvent::Input` here, rather than only calling `configure` as every
    // operator's own test above does, is what actually exercises the
    // decode-transform-encode plumbing (`ops::decode_image`/
    // `ops::encode_image`, and the `astrs_data::ipc` <-> `Image` wire round
    // trip underneath both) that every operator in this crate sits on top
    // of.

    fn input_event(payload: Vec<u8>) -> astrs_operator_api::OpEvent {
        use astrs_time::HlcTimestamp;
        use astrs_wire::{DataId, Metadata};
        astrs_operator_api::OpEvent::Input {
            id: DataId::new("in").unwrap(),
            source: "camera/image".parse().unwrap(),
            metadata: Metadata::new(HlcTimestamp::EPOCH),
            payload,
        }
    }

    /// Encodes `image` exactly as a real producer would: through
    /// [`ImageBuffer::to_message`] and [`astrs_data::ipc::encode_payload`],
    /// the same two steps [`crate::ops::decode_image`] expects on the way
    /// back in.
    fn image_payload(image: &ImageBuffer) -> Vec<u8> {
        let batch = image.to_message().unwrap().to_record_batch().unwrap();
        astrs_data::ipc::encode_payload(&batch).unwrap().to_vec()
    }

    /// The inverse of `image_payload`, via `Image::from_record_batch` --
    /// exactly what `crate::ops::decode_image` does through the
    /// `FromPayload` trait (see `astrs_node_api::message::Image`'s
    /// `FromPayload` impl, which is a one-line delegation to this same
    /// method).
    fn decode_image_payload(payload: &[u8]) -> ImageBuffer {
        let batch = astrs_data::ipc::decode_payload(payload).unwrap();
        let wire_image = astrs_node_api::message::Image::from_record_batch(&batch).unwrap();
        ImageBuffer::from_message(&wire_image).unwrap()
    }

    #[test]
    fn threshold_operator_publishes_on_the_documented_port_matching_the_direct_call() {
        use astrs_operator_api::{OpOutput, Operator, Status};

        let image = mono8(&[0, 100, 127, 128, 200, 255]);
        let mut op = ThresholdOperator::default();
        let mut config = std::collections::BTreeMap::new();
        config.insert(
            "mode".to_owned(),
            astrs_wire::Parameter::String("fixed".to_owned()),
        );
        config.insert("level".to_owned(), astrs_wire::Parameter::Integer(127));
        op.configure(&config).unwrap();

        let mut out = OpOutput::new();
        let status = op
            .on_event(&input_event(image_payload(&image)), &mut out)
            .unwrap();
        assert_eq!(status, Status::Continue);

        let sends = out.drain();
        assert_eq!(sends.len(), 1);
        assert_eq!(
            sends[0].id().as_str(),
            "thresholded",
            "must match this operator's own doc table"
        );

        let published = decode_image_payload(sends[0].payload());
        let direct = threshold_fixed(&image, 127, ThresholdMode::Normal).unwrap();
        assert_eq!(
            published, direct,
            "the operator must publish exactly what the direct function call produces"
        );
    }

    #[test]
    fn threshold_operator_finishes_on_stop_and_ignores_a_reload() {
        use astrs_operator_api::{OpEvent, OpOutput, Operator, Status};

        let mut op = ThresholdOperator::default();
        let mut out = OpOutput::new();

        let stop = OpEvent::Stop {
            cause: astrs_wire::StopCause::Requested,
            grace: None,
        };
        assert_eq!(op.on_event(&stop, &mut out).unwrap(), Status::Finished);

        assert_eq!(
            op.on_event(&OpEvent::Reload, &mut out).unwrap(),
            Status::Continue
        );
        assert!(
            out.is_empty(),
            "neither Stop nor a non-image event publishes anything"
        );
    }
}
