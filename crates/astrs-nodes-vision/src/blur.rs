//! Separable Gaussian blur, over any fully-sampled [`crate::PixelFormat`] (see
//! [`crate::error`]'s module doc — blur is a filtering op, so it works on
//! [`crate::PixelFormat::Mono8`]/[`crate::PixelFormat::Rgb8`]/[`crate::PixelFormat::Bgr8`]/
//! [`crate::PixelFormat::Rgba8`] alike, one byte plane per channel,
//! independently).
//!
//! # Separable, two passes, one rounding
//!
//! A 2-D Gaussian kernel factors exactly into the outer product of two 1-D
//! kernels, so [`gaussian_blur`] convolves every row horizontally, then
//! every column of *that* result vertically, rather than a full 2-D
//! convolution — `O(width * height * kernel_len)` work per pass instead of
//! `O(width * height * kernel_len^2)` for one. The horizontal pass's output
//! is kept in `f64` and only the vertical pass's final sum is rounded to
//! `u8`, so the two 1-D convolutions compose exactly as the one 2-D
//! convolution they represent would, with no intermediate 8-bit rounding
//! error folded into the second pass.
//!
//! Border handling is clamp-to-edge, the same convention [`crate::sobel`]
//! uses and explains in its own module doc.
//!
//! # Kernel radius
//!
//! [`gaussian_blur`] derives its kernel radius from `sigma` as `ceil(3 *
//! sigma)` (clamped to at least `1`) — the standard "three-sigma" rule of
//! thumb, beyond which a Gaussian's tail contributes well under a tenth of
//! a percent of its total weight. Two 1-D kernels of `2 * radius + 1` taps
//! each is what makes this genuinely cheaper than a `(2 * radius + 1)^2`-tap
//! 2-D kernel for the same effective blur.

use crate::buffer::ImageBuffer;
use crate::error::{Result, VisionError};

/// The one-dimensional, normalised Gaussian kernel of `2 * radius + 1`
/// taps, weight `i` (indexed from `-radius` to `radius`) proportional to
/// `exp(-i^2 / (2 * sigma^2))`.
///
/// `sigma > 0.0` and finite is the caller's responsibility (checked once,
/// in [`gaussian_blur`]) — this function has no [`Result`] of its own to
/// report a bad one through.
fn gaussian_kernel(sigma: f64, radius: usize) -> Vec<f64> {
    let denom = 2.0 * sigma * sigma;
    let mut weights: Vec<f64> = (0..=2 * radius)
        .map(|i| {
            let offset = i as f64 - radius as f64;
            (-(offset * offset) / denom).exp()
        })
        .collect();
    let sum: f64 = weights.iter().sum();
    for weight in &mut weights {
        *weight /= sum;
    }
    weights
}

/// The standard "three-sigma" kernel radius: `ceil(3 * sigma)`, at least
/// `1` so even a very small `sigma` still produces a (near-identity)
/// 3-tap kernel rather than a single-tap no-op.
fn auto_radius(sigma: f64) -> usize {
    ((3.0 * sigma).ceil() as usize).max(1)
}

/// Convolves one axis of `plane` (row-major, `width`x`height`, `bpp`
/// channels per pixel) with `kernel`, clamping out-of-range taps to the
/// nearest edge sample. `along_x` selects which axis; the other axis's
/// index is passed straight through.
fn convolve_axis(
    plane: &[f64],
    width: usize,
    height: usize,
    bpp: usize,
    kernel: &[f64],
    along_x: bool,
) -> Vec<f64> {
    let radius = (kernel.len() - 1) / 2;
    let mut out = vec![0.0f64; plane.len()];
    for y in 0..height {
        for x in 0..width {
            for c in 0..bpp {
                let mut acc = 0.0f64;
                for (tap, &weight) in kernel.iter().enumerate() {
                    let offset = tap as i64 - radius as i64;
                    let (sx, sy) = if along_x {
                        (clamp_axis(x as i64 + offset, width), y)
                    } else {
                        (x, clamp_axis(y as i64 + offset, height))
                    };
                    acc += weight * plane[(sy * width + sx) * bpp + c];
                }
                out[(y * width + x) * bpp + c] = acc;
            }
        }
    }
    out
}

/// Clamps a possibly out-of-range signed coordinate into `0..len` — the
/// same edge-replication [`crate::sobel`] uses.
fn clamp_axis(value: i64, len: usize) -> usize {
    value.clamp(0, len as i64 - 1) as usize
}

/// Blurs `image` with a Gaussian of standard deviation `sigma` (in pixels).
///
/// A `0`x`0` frame blurs to itself (nothing to convolve).
///
/// # Errors
///
/// [`crate::VisionError::UnsupportedFormat`] for a packed 4:2:2 source
/// ([`crate::PixelFormat::Yuyv`]/[`crate::PixelFormat::Uyvy`]);
/// [`crate::VisionError::InvalidParameter`] (`what: "gaussian sigma"`) when
/// `sigma` is not finite and strictly positive.
pub fn gaussian_blur(image: &ImageBuffer, sigma: f64) -> Result<ImageBuffer> {
    image.require_fully_sampled("blur::gaussian_blur")?;
    if !(sigma.is_finite() && sigma > 0.0) {
        return Err(VisionError::InvalidParameter {
            what: "gaussian sigma",
            reason: "sigma must be finite and strictly greater than zero",
        });
    }

    let width = image.width() as usize;
    let height = image.height() as usize;
    if width == 0 || height == 0 {
        return Ok(image.clone());
    }

    let bpp = image.bytes_per_pixel();
    let row_bytes = image.row_bytes();
    let radius = auto_radius(sigma);
    let kernel = gaussian_kernel(sigma, radius);

    let data = image.data();
    let source: Vec<f64> = (0..height)
        .flat_map(|y| {
            let row_start = y * row_bytes;
            data[row_start..row_start + width * bpp]
                .iter()
                .map(|&b| f64::from(b))
        })
        .collect();

    let horizontal = convolve_axis(&source, width, height, bpp, &kernel, true);
    let vertical = convolve_axis(&horizontal, width, height, bpp, &kernel, false);

    // Every tap weight is non-negative and the kernel sums to (within
    // float epsilon of) 1.0, so `vertical`'s entries are convex
    // combinations of `u8` samples and therefore already within
    // `0.0..=255.0` up to that same epsilon; `as u8` on a float saturates
    // (stable Rust cast semantics) rather than wrapping, so no explicit
    // clamp is needed even for the epsilon overshoot.
    let out: Vec<u8> = vertical.iter().map(|&v| v.round() as u8).collect();
    ImageBuffer::new(image.format(), image.width(), image.height(), out)
}

/// An [`astrs_operator_api::Operator`] applying [`gaussian_blur`] to every
/// `image` input, publishing the blurred frame on `blurred`.
///
/// # Configuration
///
/// | Key | Type | Default | Meaning |
/// |---|---|---|---|
/// | `sigma` | float | `1.0` | The Gaussian standard deviation, in pixels |
#[derive(Debug)]
pub struct GaussianBlurOperator {
    sigma: f64,
}

impl Default for GaussianBlurOperator {
    fn default() -> Self {
        Self { sigma: 1.0 }
    }
}

impl astrs_operator_api::Operator for GaussianBlurOperator {
    fn configure(
        &mut self,
        config: &std::collections::BTreeMap<String, astrs_wire::Parameter>,
    ) -> astrs_operator_api::OpResult<()> {
        if let Some(value) = config
            .get("sigma")
            .and_then(astrs_wire::Parameter::as_float)
        {
            self.sigma = value;
        }
        Ok(())
    }

    fn on_event(
        &mut self,
        event: &astrs_operator_api::OpEvent,
        out: &mut astrs_operator_api::OpOutput,
    ) -> astrs_operator_api::OpResult<astrs_operator_api::Status> {
        crate::ops::forward_image(event, out, "blurred", |image| {
            gaussian_blur(image, self.sigma)
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
    fn kernel_is_normalized_symmetric_and_peaks_at_the_centre() {
        for sigma in [0.5, 1.0, 2.0, 5.0] {
            let radius = auto_radius(sigma);
            let kernel = gaussian_kernel(sigma, radius);
            assert_eq!(kernel.len(), 2 * radius + 1);

            let sum: f64 = kernel.iter().sum();
            assert!((sum - 1.0).abs() < 1e-9, "sigma {sigma}: kernel sum {sum}");

            for i in 0..kernel.len() {
                assert!(
                    (kernel[i] - kernel[kernel.len() - 1 - i]).abs() < 1e-12,
                    "sigma {sigma}: index {i} not symmetric"
                );
            }

            let center = kernel[radius];
            assert!(
                kernel.iter().all(|&w| w <= center + 1e-15),
                "sigma {sigma}: centre tap should be the largest weight"
            );
        }
    }

    #[test]
    fn kernel_weights_match_the_gaussian_formula_independently_recomputed() {
        let sigma = 1.0;
        let radius = 2;
        let kernel = gaussian_kernel(sigma, radius);
        let raw: Vec<f64> = (-2i64..=2)
            .map(|i| (-((i * i) as f64) / (2.0 * sigma * sigma)).exp())
            .collect();
        let sum: f64 = raw.iter().sum();
        for (got, &r) in kernel.iter().zip(raw.iter()) {
            assert!(
                (got - r / sum).abs() < 1e-12,
                "got {got}, expected {}",
                r / sum
            );
        }
    }

    #[test]
    fn auto_radius_follows_the_three_sigma_rule() {
        assert_eq!(auto_radius(0.1), 1); // ceil(0.3) = 1
        assert_eq!(auto_radius(1.0), 3); // ceil(3.0) = 3
        assert_eq!(auto_radius(2.0), 6); // ceil(6.0) = 6
    }

    #[test]
    fn auto_radius_floors_at_one_even_for_a_degenerate_sigma() {
        // `sigma <= 0.0` never reaches this function through the public
        // `gaussian_blur` (rejected first), but the floor is still what
        // keeps a hypothetical zero-tap kernel from underflowing
        // `convolve_axis`'s `(kernel.len() - 1) / 2` radius computation.
        assert_eq!(auto_radius(0.0), 1);
    }

    #[test]
    fn blurring_a_flat_image_is_the_identity() {
        let flat = mono8(&[100; 25], 5, 5);
        assert_eq!(gaussian_blur(&flat, 1.5).unwrap(), flat);

        let flat_rgb = ImageBuffer::new(PixelFormat::Rgb8, 3, 3, [50, 60, 70].repeat(9)).unwrap();
        assert_eq!(gaussian_blur(&flat_rgb, 0.8).unwrap(), flat_rgb);
    }

    #[test]
    fn an_impulse_spreads_symmetrically_and_stays_the_maximum() {
        let mut data = vec![0u8; 25];
        data[12] = 255; // (2, 2) of a 5x5 frame
        let image = mono8(&data, 5, 5);
        let blurred = gaussian_blur(&image, 1.0).unwrap();

        let center = blurred.pixel(2, 2).unwrap()[0];
        assert!(center < 255, "energy should have spread away from the peak");
        assert!(center > 0, "the peak itself should still carry some weight");
        assert!(
            blurred.data().iter().all(|&v| v <= center),
            "the peak should stay the maximum"
        );

        assert_eq!(
            blurred.pixel(1, 2),
            blurred.pixel(3, 2),
            "symmetric left/right of the peak"
        );
        assert_eq!(
            blurred.pixel(0, 2),
            blurred.pixel(4, 2),
            "symmetric at the frame edges too"
        );
        assert_eq!(
            blurred.pixel(2, 1),
            blurred.pixel(2, 3),
            "symmetric above/below the peak"
        );
    }

    #[test]
    fn blur_refuses_a_packed_yuyv_source() {
        let yuyv = ImageBuffer::zeroed(PixelFormat::Yuyv, 2, 1).unwrap();
        assert!(matches!(
            gaussian_blur(&yuyv, 1.0),
            Err(VisionError::UnsupportedFormat {
                op: "blur::gaussian_blur",
                ..
            })
        ));
    }

    #[test]
    fn blur_rejects_a_non_positive_or_non_finite_sigma() {
        let image = mono8(&[1, 2, 3, 4], 2, 2);
        for sigma in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                matches!(
                    gaussian_blur(&image, sigma),
                    Err(VisionError::InvalidParameter {
                        what: "gaussian sigma",
                        ..
                    })
                ),
                "sigma {sigma} should be rejected"
            );
        }
    }

    #[test]
    fn blurring_an_empty_frame_is_the_identity() {
        let empty = ImageBuffer::zeroed(PixelFormat::Mono8, 0, 0).unwrap();
        assert_eq!(gaussian_blur(&empty, 1.0).unwrap(), empty);
    }
}
