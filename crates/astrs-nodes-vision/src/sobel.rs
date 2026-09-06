//! Sobel gradients (spec: Sobel & Feldman, 1968) — the 3x3 horizontal and
//! vertical derivative-approximating kernels every classic edge detector
//! starts from.
//!
//! [`sobel_gradients`] is the primitive: it returns the full-precision
//! `Gx`/`Gy` planes as [`SobelGradients`], since a caller building anything
//! beyond a magnitude image (edge direction, non-maximum suppression, a
//! Harris-corner response) needs the signed components, not just their
//! combined size. [`sobel_magnitude`] is the common case — `sqrt(Gx^2 +
//! Gy^2)`, clamped into a displayable [`PixelFormat::Mono8`] frame — built
//! on top of it.
//!
//! # Border handling: clamp-to-edge
//!
//! A 3x3 kernel centred on a border pixel reaches one column/row past the
//! frame. This module extends the frame by replicating its edge pixels
//! (`x.clamp(0, width - 1)`) rather than treating the border as zero —
//! zero-padding manufactures a fake edge at the image boundary itself,
//! which is a worse answer for a frame whose border pixels are usually just
//! more of the same scene. [`crate::blur`]'s separable convolution uses the
//! same convention, for the same reason.

use crate::buffer::ImageBuffer;
use crate::error::Result;
use crate::pixel::PixelFormat;

/// The horizontal Sobel kernel, row-major, top row first.
const KERNEL_X: [[i32; 3]; 3] = [[-1, 0, 1], [-2, 0, 2], [-1, 0, 1]];

/// The vertical Sobel kernel, row-major, top row first — `KERNEL_X`
/// rotated 90 degrees.
const KERNEL_Y: [[i32; 3]; 3] = [[-1, -2, -1], [0, 0, 0], [1, 2, 1]];

/// The full-precision `Gx`/`Gy` gradient planes [`sobel_gradients`]
/// computes over one [`PixelFormat::Mono8`] frame.
///
/// Each plane is row-major, one `i16` per source pixel — signed, since a
/// gradient component is: a Sobel response over 8-bit samples never exceeds
/// `+-1020` (four samples at weight `+-1` or two at `+-2`, each up to
/// `255`), which fits [`i16`] with headroom to spare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SobelGradients {
    width: u32,
    height: u32,
    gx: Vec<i16>,
    gy: Vec<i16>,
}

impl SobelGradients {
    /// The source frame's width.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The source frame's height.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The horizontal gradient plane, row-major.
    #[must_use]
    pub fn gx(&self) -> &[i16] {
        &self.gx
    }

    /// The vertical gradient plane, row-major.
    #[must_use]
    pub fn gy(&self) -> &[i16] {
        &self.gy
    }

    /// The `(Gx, Gy)` pair at `(x, y)`, or [`None`] outside the frame.
    #[must_use]
    pub fn at(&self, x: u32, y: u32) -> Option<(i16, i16)> {
        let index = pixel_index(x, y, self.width, self.height)?;
        Some((self.gx[index], self.gy[index]))
    }

    /// The gradient magnitude at `(x, y)` — `sqrt(Gx^2 + Gy^2)`, computed in
    /// `f64` — or [`None`] outside the frame.
    #[must_use]
    pub fn magnitude_at(&self, x: u32, y: u32) -> Option<f64> {
        let (gx, gy) = self.at(x, y)?;
        Some(magnitude(gx, gy))
    }
}

/// `sqrt(Gx^2 + Gy^2)`, widened to `f64` before squaring so the intermediate
/// never risks overflowing — not that it could here (`i16::MAX^2 * 2` fits
/// `i32` too), but every arithmetic step in this crate is exact-or-checked
/// on principle.
fn magnitude(gx: i16, gy: i16) -> f64 {
    let gx = f64::from(gx);
    let gy = f64::from(gy);
    gx.hypot(gy)
}

/// The flat row-major index of `(x, y)` in a `width`x`height` frame, or
/// [`None`] outside it.
fn pixel_index(x: u32, y: u32, width: u32, height: u32) -> Option<usize> {
    if x >= width || y >= height {
        return None;
    }
    Some((y as usize) * (width as usize) + (x as usize))
}

/// Clamps a possibly out-of-range signed neighbour coordinate into
/// `0..len`, replicating the edge sample (see this module's doc).
///
/// `len == 0` has no valid clamp target; callers never reach this with a
/// zero-sized frame (see [`sobel_gradients`]'s early return).
fn clamp_coord(value: i64, len: u32) -> usize {
    value.clamp(0, i64::from(len) - 1) as usize
}

/// Computes the Sobel `Gx`/`Gy` gradient planes of a [`PixelFormat::Mono8`]
/// frame.
///
/// A `0`x`0` or otherwise empty frame yields empty, valid planes rather than
/// an error — there is no pixel to compute a gradient at, which is a
/// perfectly good (if vacuous) answer.
///
/// # Errors
///
/// [`crate::VisionError::UnsupportedFormat`] when `image.format()` is not
/// [`PixelFormat::Mono8`].
pub fn sobel_gradients(image: &ImageBuffer) -> Result<SobelGradients> {
    image.require_format(PixelFormat::Mono8, "sobel::sobel_gradients")?;
    let width = image.width();
    let height = image.height();
    let data = image.data();
    let w = width as usize;
    let h = height as usize;
    let mut gx = vec![0i16; w * h];
    let mut gy = vec![0i16; w * h];

    if w == 0 || h == 0 {
        return Ok(SobelGradients {
            width,
            height,
            gx,
            gy,
        });
    }

    for y in 0..h {
        for x in 0..w {
            let mut sum_x = 0i32;
            let mut sum_y = 0i32;
            for (ky, (row_x, row_y)) in KERNEL_X.iter().zip(KERNEL_Y.iter()).enumerate() {
                let sample_y = clamp_coord(y as i64 + ky as i64 - 1, height);
                for (kx, (&wx, &wy)) in row_x.iter().zip(row_y.iter()).enumerate() {
                    let sample_x = clamp_coord(x as i64 + kx as i64 - 1, width);
                    let sample = i32::from(data[sample_y * w + sample_x]);
                    sum_x += wx * sample;
                    sum_y += wy * sample;
                }
            }
            // `sum_x`/`sum_y` are bounded to `+-1020` (see `SobelGradients`'s
            // doc), well inside `i16`.
            gx[y * w + x] = sum_x as i16;
            gy[y * w + x] = sum_y as i16;
        }
    }

    Ok(SobelGradients {
        width,
        height,
        gx,
        gy,
    })
}

/// Computes the Sobel gradient magnitude of a [`PixelFormat::Mono8`] frame,
/// as a fresh `Mono8` frame of the same size.
///
/// Each output byte is `sqrt(Gx^2 + Gy^2)` (see [`SobelGradients`]),
/// saturating at `255` — a magnitude above that is still "strong edge", and
/// saturating (rather than, say, halving every value to fit) keeps a
/// hand-computed golden image simple to check.
///
/// # Errors
///
/// As [`sobel_gradients`].
pub fn sobel_magnitude(image: &ImageBuffer) -> Result<ImageBuffer> {
    let gradients = sobel_gradients(image)?;
    let mut out = Vec::with_capacity(gradients.gx.len());
    for (&gx, &gy) in gradients.gx.iter().zip(gradients.gy.iter()) {
        out.push(magnitude(gx, gy).min(255.0) as u8);
    }
    ImageBuffer::new(PixelFormat::Mono8, gradients.width, gradients.height, out)
}

/// An [`astrs_operator_api::Operator`] applying [`sobel_magnitude`] to every
/// `image` input, publishing the gradient-magnitude frame on `edges`.
///
/// Takes no configuration — the 3x3 kernels and clamp-to-edge border
/// convention are the whole of what Sobel means, so there is nothing left
/// to make caller-adjustable.
#[derive(Debug, Default)]
pub struct SobelOperator;

impl astrs_operator_api::Operator for SobelOperator {
    fn on_event(
        &mut self,
        event: &astrs_operator_api::OpEvent,
        out: &mut astrs_operator_api::OpOutput,
    ) -> astrs_operator_api::OpResult<astrs_operator_api::Status> {
        crate::ops::forward_image(event, out, "edges", sobel_magnitude)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::error::VisionError;

    fn mono8(data: &[u8], width: u32, height: u32) -> ImageBuffer {
        ImageBuffer::new(PixelFormat::Mono8, width, height, data.to_vec()).unwrap()
    }

    /// A 3x3 frame with a clean vertical edge (columns `10 | 10 | 200`) —
    /// hand-verified in this module's own doc-adjacent derivation: with
    /// clamp-to-edge borders, the centre pixel's `Gx` is
    /// `(-10+200)*1 + (-10+200)*2 + (-10+200)*1 = 190*4 = 760`, and `Gy` is
    /// `0` (every row is identical, so the vertical kernel sees no
    /// variation at all).
    fn vertical_edge() -> ImageBuffer {
        #[rustfmt::skip]
        let data = [
            10, 10, 200,
            10, 10, 200,
            10, 10, 200,
        ];
        mono8(&data, 3, 3)
    }

    /// The same edge, rotated 90 degrees: rows `10 | 10 | 200`. By the same
    /// arithmetic (kernels swap roles under a 90-degree rotation), the
    /// centre pixel's `Gy` is `760` and `Gx` is `0`.
    fn horizontal_edge() -> ImageBuffer {
        #[rustfmt::skip]
        let data = [
            10, 10, 10,
            10, 10, 10,
            200, 200, 200,
        ];
        mono8(&data, 3, 3)
    }

    #[test]
    fn a_vertical_edge_produces_a_pure_horizontal_gradient_at_the_centre() {
        let gradients = sobel_gradients(&vertical_edge()).unwrap();
        assert_eq!(gradients.at(1, 1), Some((760, 0)));
    }

    #[test]
    fn a_horizontal_edge_produces_a_pure_vertical_gradient_at_the_centre() {
        let gradients = sobel_gradients(&horizontal_edge()).unwrap();
        assert_eq!(gradients.at(1, 1), Some((0, 760)));
    }

    #[test]
    fn a_flat_image_has_zero_gradient_everywhere() {
        let flat = mono8(&[42; 9], 3, 3);
        let gradients = sobel_gradients(&flat).unwrap();
        assert!(gradients.gx().iter().all(|&v| v == 0));
        assert!(gradients.gy().iter().all(|&v| v == 0));
    }

    #[test]
    fn magnitude_matches_the_pythagorean_combination() {
        let gradients = sobel_gradients(&vertical_edge()).unwrap();
        assert_eq!(gradients.magnitude_at(1, 1), Some(760.0));
        assert_eq!(gradients.magnitude_at(99, 99), None);
    }

    #[test]
    fn sobel_magnitude_saturates_a_strong_edge_at_255() {
        let magnitude = sobel_magnitude(&vertical_edge()).unwrap();
        assert_eq!(magnitude.format(), PixelFormat::Mono8);
        // 760 saturates well past 255.
        assert_eq!(magnitude.pixel(1, 1), Some(&[255u8][..]));
    }

    #[test]
    fn sobel_refuses_a_non_mono8_frame() {
        let rgb = ImageBuffer::zeroed(PixelFormat::Rgb8, 2, 2).unwrap();
        assert!(matches!(
            sobel_gradients(&rgb),
            Err(VisionError::UnsupportedFormat {
                op: "sobel::sobel_gradients",
                ..
            })
        ));
    }

    #[test]
    fn an_empty_frame_yields_empty_planes_not_an_error() {
        let empty = ImageBuffer::zeroed(PixelFormat::Mono8, 0, 0).unwrap();
        let gradients = sobel_gradients(&empty).unwrap();
        assert!(gradients.gx().is_empty());
        assert!(gradients.gy().is_empty());
    }

    #[test]
    fn a_single_pixel_frame_has_zero_gradient() {
        // With clamp-to-edge borders every one of the nine kernel taps
        // samples the same single pixel, so both kernels see zero variation.
        let single = mono8(&[100], 1, 1);
        let gradients = sobel_gradients(&single).unwrap();
        assert_eq!(gradients.at(0, 0), Some((0, 0)));
    }
}
