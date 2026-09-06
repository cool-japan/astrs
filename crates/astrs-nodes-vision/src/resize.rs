//! Resizing: nearest-neighbour and bilinear, over any fully-sampled
//! [`crate::PixelFormat`] (see [`crate::error`]'s module doc — resize is a
//! geometric op, so it works on [`crate::PixelFormat::Mono8`]/
//! [`crate::PixelFormat::Rgb8`]/[`crate::PixelFormat::Bgr8`]/
//! [`crate::PixelFormat::Rgba8`] alike, one byte plane per channel,
//! independently).
//!
//! # Sampling conventions, pinned so a golden test has one right answer
//!
//! [`resize_nearest`] maps an output column `ox` to source column `ox *
//! src_width / new_width` (integer division, no pixel-centre offset) — the
//! simplest nearest-neighbour mapping, and the one whose output is easiest
//! to hand-verify (an integer upscale replicates each source pixel an exact
//! number of times; an integer downscale picks an exact stride of source
//! pixels).
//!
//! [`resize_bilinear`] instead uses the "half-pixel-centre" convention
//! common to OpenCV's `INTER_LINEAR` and PyTorch's `align_corners=false`:
//! output pixel `ox`'s centre maps to source coordinate `(ox + 0.5) *
//! (src_width / new_width) - 0.5`, clamped into `0..=src_width - 1` before
//! the surrounding two source samples and their interpolation weight are
//! read off it. The practical effect: resizing to the *same* size is the
//! identity (every output centre lands exactly on its source pixel), which
//! [`resize_nearest`]'s simpler mapping does not guarantee for a downscale.
//!
//! # Zero-sized frames
//!
//! Resizing *to* `0`x`0` always succeeds (an empty [`ImageBuffer`] in the
//! source's format — nothing to sample, nothing to produce).
//! Resizing a `0`x`0` *source* to a non-empty target has no source pixel to
//! read at all, and is rejected with
//! [`crate::VisionError::InvalidParameter`] rather than manufacturing a
//! black frame.

use crate::buffer::ImageBuffer;
use crate::error::{Result, VisionError};

/// How [`ResizeOperator`] samples the source frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResizeMode {
    /// [`resize_nearest`].
    Nearest,
    /// [`resize_bilinear`] — the default.
    #[default]
    Bilinear,
}

/// Rejects a resize whose source has no pixels but whose target wants some.
fn check_nonempty_source(image: &ImageBuffer, new_width: u32, new_height: u32) -> Result<()> {
    let source_empty = image.width() == 0 || image.height() == 0;
    let target_empty = new_width == 0 || new_height == 0;
    if source_empty && !target_empty {
        return Err(VisionError::InvalidParameter {
            what: "resize target",
            reason: "cannot resize an empty source image to a non-empty target",
        });
    }
    Ok(())
}

/// Resizes `image` to `new_width`x`new_height` by nearest-neighbour sampling
/// (see this module's doc for the exact mapping).
///
/// # Errors
///
/// [`crate::VisionError::UnsupportedFormat`] for a packed 4:2:2 source
/// ([`crate::PixelFormat::Yuyv`]/[`crate::PixelFormat::Uyvy`]);
/// [`crate::VisionError::InvalidParameter`] when `image` is empty and the
/// target is not.
pub fn resize_nearest(image: &ImageBuffer, new_width: u32, new_height: u32) -> Result<ImageBuffer> {
    image.require_fully_sampled("resize::resize_nearest")?;
    check_nonempty_source(image, new_width, new_height)?;
    let format = image.format();
    if new_width == 0 || new_height == 0 {
        return ImageBuffer::new(format, new_width, new_height, Vec::new());
    }

    let bpp = image.bytes_per_pixel();
    let row_bytes = image.row_bytes();
    let src_width = image.width();
    let src_height = image.height();
    let data = image.data();

    let mut out = Vec::with_capacity((new_width as usize) * (new_height as usize) * bpp);
    for oy in 0..new_height {
        let sy = (u64::from(oy) * u64::from(src_height) / u64::from(new_height)) as usize;
        let row_start = sy * row_bytes;
        for ox in 0..new_width {
            let sx = (u64::from(ox) * u64::from(src_width) / u64::from(new_width)) as usize;
            let pixel_start = row_start + sx * bpp;
            out.extend_from_slice(&data[pixel_start..pixel_start + bpp]);
        }
    }
    ImageBuffer::new(format, new_width, new_height, out)
}

/// The `(low_index, high_index, fraction)` triple [`resize_bilinear`] reads
/// one output coordinate's two source samples and interpolation weight
/// from — see this module's doc for the half-pixel-centre convention.
///
/// `src_len <= 1` is degenerate (nothing to interpolate between): both
/// indices are `0` and the fraction is `0.0`, so the one source sample is
/// reproduced exactly regardless of `out_len`.
fn source_span(out_index: u32, out_len: u32, src_len: u32) -> (usize, usize, f64) {
    if src_len <= 1 {
        return (0, 0, 0.0);
    }
    let scale = f64::from(src_len) / f64::from(out_len);
    let raw = (f64::from(out_index) + 0.5) * scale - 0.5;
    let clamped = raw.clamp(0.0, f64::from(src_len - 1));
    let low = clamped.floor();
    let fraction = clamped - low;
    let low_index = low as usize;
    let high_index = (low_index + 1).min(src_len as usize - 1);
    (low_index, high_index, fraction)
}

/// Linear interpolation between two samples, rounded to the nearest `u8`.
/// `t` is always in `0.0..=1.0` (from [`source_span`]), so the result never
/// leaves `min(a, b)..=max(a, b)` — no clamp needed.
fn lerp_u8(a: u8, b: u8, t: f64) -> u8 {
    (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8
}

/// Resizes `image` to `new_width`x`new_height` by bilinear sampling (see
/// this module's doc for the exact half-pixel-centre convention).
///
/// # Errors
///
/// As [`resize_nearest`].
pub fn resize_bilinear(
    image: &ImageBuffer,
    new_width: u32,
    new_height: u32,
) -> Result<ImageBuffer> {
    image.require_fully_sampled("resize::resize_bilinear")?;
    check_nonempty_source(image, new_width, new_height)?;
    let format = image.format();
    if new_width == 0 || new_height == 0 {
        return ImageBuffer::new(format, new_width, new_height, Vec::new());
    }

    let bpp = image.bytes_per_pixel();
    let row_bytes = image.row_bytes();
    let src_width = image.width();
    let src_height = image.height();
    let data = image.data();

    let mut out = Vec::with_capacity((new_width as usize) * (new_height as usize) * bpp);
    for oy in 0..new_height {
        let (y0, y1, fy) = source_span(oy, new_height, src_height);
        let row0 = y0 * row_bytes;
        let row1 = y1 * row_bytes;
        for ox in 0..new_width {
            let (x0, x1, fx) = source_span(ox, new_width, src_width);
            for channel in 0..bpp {
                let top_left = data[row0 + x0 * bpp + channel];
                let top_right = data[row0 + x1 * bpp + channel];
                let bottom_left = data[row1 + x0 * bpp + channel];
                let bottom_right = data[row1 + x1 * bpp + channel];
                let top = lerp_u8(top_left, top_right, fx);
                let bottom = lerp_u8(bottom_left, bottom_right, fx);
                out.push(lerp_u8(top, bottom, fy));
            }
        }
    }
    ImageBuffer::new(format, new_width, new_height, out)
}

/// An [`astrs_operator_api::Operator`] applying [`resize_nearest`] or
/// [`resize_bilinear`] to every `image` input, publishing the resized frame
/// on `resized`.
///
/// # Configuration
///
/// | Key | Type | Default | Meaning |
/// |---|---|---|---|
/// | `width` | integer | required | Target width in pixels, `0..=u32::MAX` |
/// | `height` | integer | required | Target height in pixels, `0..=u32::MAX` |
/// | `mode` | string | `"bilinear"` | `"nearest"` or `"bilinear"` |
#[derive(Debug, Default)]
pub struct ResizeOperator {
    width: u32,
    height: u32,
    mode: ResizeMode,
}

/// Reads one dimension's `u32` config value, or fails with the same
/// [`astrs_operator_api::OpError::Failed`] shape for a missing or
/// out-of-range value.
fn config_dimension(
    config: &std::collections::BTreeMap<String, astrs_wire::Parameter>,
    key: &str,
) -> astrs_operator_api::OpResult<u32> {
    let value = config
        .get(key)
        .and_then(astrs_wire::Parameter::as_integer)
        .ok_or_else(|| {
            astrs_operator_api::OpError::failed(format!("resize requires an integer {key:?}"))
        })?;
    u32::try_from(value).map_err(|_| {
        astrs_operator_api::OpError::failed(format!(
            "resize {key} must fit in 0..=u32::MAX, got {value}"
        ))
    })
}

impl astrs_operator_api::Operator for ResizeOperator {
    fn configure(
        &mut self,
        config: &std::collections::BTreeMap<String, astrs_wire::Parameter>,
    ) -> astrs_operator_api::OpResult<()> {
        self.width = config_dimension(config, "width")?;
        self.height = config_dimension(config, "height")?;
        self.mode = match config.get("mode").and_then(astrs_wire::Parameter::as_str) {
            Some("nearest") => ResizeMode::Nearest,
            Some("bilinear") | None => ResizeMode::Bilinear,
            Some(other) => {
                return Err(astrs_operator_api::OpError::failed(format!(
                    "resize mode must be \"nearest\" or \"bilinear\", got {other:?}"
                )));
            }
        };
        Ok(())
    }

    fn on_event(
        &mut self,
        event: &astrs_operator_api::OpEvent,
        out: &mut astrs_operator_api::OpOutput,
    ) -> astrs_operator_api::OpResult<astrs_operator_api::Status> {
        crate::ops::forward_image(event, out, "resized", |image| match self.mode {
            ResizeMode::Nearest => resize_nearest(image, self.width, self.height),
            ResizeMode::Bilinear => resize_bilinear(image, self.width, self.height),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::pixel::PixelFormat;

    fn mono8(data: &[u8], width: u32, height: u32) -> ImageBuffer {
        ImageBuffer::new(PixelFormat::Mono8, width, height, data.to_vec()).unwrap()
    }

    #[test]
    fn nearest_upscale_replicates_each_source_pixel() {
        let image = mono8(&[1, 2, 3, 4], 2, 2);
        let resized = resize_nearest(&image, 4, 4).unwrap();
        #[rustfmt::skip]
        let expected = [
            1, 1, 2, 2,
            1, 1, 2, 2,
            3, 3, 4, 4,
            3, 3, 4, 4,
        ];
        assert_eq!(resized.data(), &expected);
    }

    #[test]
    fn nearest_downscale_picks_an_exact_stride() {
        #[rustfmt::skip]
        let image = mono8(&[
            1, 2, 3, 4,
            5, 6, 7, 8,
            9, 10, 11, 12,
            13, 14, 15, 16,
        ], 4, 4);
        let resized = resize_nearest(&image, 2, 2).unwrap();
        // Columns/rows 0 and 2 of the source (ox*4/2: 0->0, 1->2).
        assert_eq!(resized.data(), &[1, 3, 9, 11]);
    }

    #[test]
    fn nearest_to_the_same_size_is_the_identity() {
        let image = mono8(&[1, 2, 3, 4, 5, 6], 3, 2);
        assert_eq!(resize_nearest(&image, 3, 2).unwrap(), image);
    }

    #[test]
    fn bilinear_to_the_same_size_is_the_identity() {
        let image = mono8(&[10, 200, 30, 255, 5, 6], 3, 2);
        assert_eq!(resize_bilinear(&image, 3, 2).unwrap(), image);
    }

    #[test]
    fn bilinear_upscale_matches_the_hand_worked_half_pixel_mapping() {
        // 2x1 image [0, 100] -> 4x1: see this module's own doc-adjacent
        // derivation. Source coords for ox=0..4 are -0.25 (clamped to 0),
        // 0.25, 0.75, 1.25 (clamped to 1); lerp(0, 100, t) at those four
        // t-values is 0, 25, 75, 100.
        let image = mono8(&[0, 100], 2, 1);
        let resized = resize_bilinear(&image, 4, 1).unwrap();
        assert_eq!(resized.data(), &[0, 25, 75, 100]);
    }

    #[test]
    fn bilinear_interpolates_every_channel_of_a_multi_channel_format() {
        let image = ImageBuffer::new(PixelFormat::Rgb8, 2, 1, vec![0, 0, 0, 100, 200, 50]).unwrap();
        let resized = resize_bilinear(&image, 4, 1).unwrap();
        // Same t-values as the mono8 case (0, 0.25, 0.75, 1.0), applied
        // independently per channel. The blue channel's t=0.25 sample is
        // 50*0.25 = 12.5 exactly, which `f64::round` breaks away from zero
        // (to 13), not down to 12 — the one non-obvious value here.
        assert_eq!(
            resized.data(),
            &[0, 0, 0, 25, 50, 13, 75, 150, 38, 100, 200, 50]
        );
    }

    #[test]
    fn resize_refuses_a_packed_yuyv_source() {
        let yuyv = ImageBuffer::zeroed(PixelFormat::Yuyv, 2, 1).unwrap();
        assert!(matches!(
            resize_nearest(&yuyv, 4, 1),
            Err(VisionError::UnsupportedFormat {
                op: "resize::resize_nearest",
                ..
            })
        ));
        assert!(matches!(
            resize_bilinear(&yuyv, 4, 1),
            Err(VisionError::UnsupportedFormat {
                op: "resize::resize_bilinear",
                ..
            })
        ));
    }

    #[test]
    fn resizing_to_zero_by_zero_always_succeeds() {
        let image = mono8(&[1, 2, 3, 4], 2, 2);
        let resized = resize_nearest(&image, 0, 0).unwrap();
        assert_eq!((resized.width(), resized.height()), (0, 0));
        assert!(resized.data().is_empty());
    }

    #[test]
    fn resizing_an_empty_source_to_a_non_empty_target_is_rejected() {
        let empty = ImageBuffer::zeroed(PixelFormat::Mono8, 0, 0).unwrap();
        assert!(matches!(
            resize_nearest(&empty, 4, 4),
            Err(VisionError::InvalidParameter {
                what: "resize target",
                ..
            })
        ));
    }

    #[test]
    fn resizing_an_empty_source_to_an_empty_target_succeeds() {
        let empty = ImageBuffer::zeroed(PixelFormat::Mono8, 0, 0).unwrap();
        assert_eq!(resize_nearest(&empty, 0, 5).unwrap().data().len(), 0);
    }
}
