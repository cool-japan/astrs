//! Pinhole camera intrinsics, plumb-bob (Brown-Conrady) lens distortion,
//! and the [`undistort_map`]/[`remap`] pair that turns the two into a
//! rectified image — over any fully-sampled [`crate::PixelFormat`] (see
//! [`crate::error`]'s module doc).
//!
//! # The pipeline, and which direction each step runs
//!
//! A real lens *distorts*: [`Distortion::distort_normalized`] models that
//! forward direction, on normalised (intrinsics-free) coordinates. Turning
//! a captured (distorted) frame back into a rectified one runs the whole
//! pipeline **backwards from the output**, the same direction OpenCV's
//! `initUndistortRectifyMap` uses: for every pixel `(u, v)` of the
//! *rectified* image you want to produce, [`Intrinsics::unproject_to_normalized`]
//! finds the ideal ray that pixel represents, [`Distortion::distort_normalized`]
//! finds where a real lens would actually have bent that ray to, and
//! [`Intrinsics::project_normalized`] turns that back into a pixel
//! coordinate — in the *original, distorted* frame, which is exactly the
//! source coordinate to sample. Getting this backwards (distorting the
//! output coordinate instead of un-distorting it) produces a
//! plausible-looking but wrong image that a round-trip test alone would
//! not catch, which is why this module's own tests pin two
//! distortion-independent invariants instead (see [`undistort_map`]'s
//! doc).
//!
//! [`undistort_map`] runs that three-step pipeline once per output pixel
//! and caches the result as a [`UndistortMap`]; [`remap`] is the cheap part
//! that runs every frame after, sampling the *same* map against however
//! many frames a fixed camera produces. [`undistort`] is the two glued
//! together for a caller with no reason to keep the map itself.
//!
//! This module deliberately does not model a separate "new camera matrix"
//! or a rectification rotation (stereo rectification's `R`) — the source
//! and destination share one [`Intrinsics`], which is the common
//! single-camera undistort case and keeps the three-step pipeline above to
//! exactly three steps.

use crate::buffer::ImageBuffer;
use crate::error::{Result, VisionError};

/// Pinhole camera intrinsics, in pixels: focal lengths `(fx, fy)` and
/// principal point `(cx, cy)`. No skew term — every sensor this crate's
/// pipeline targets has square, axis-aligned pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Intrinsics {
    /// Horizontal focal length, in pixels.
    pub fx: f64,
    /// Vertical focal length, in pixels.
    pub fy: f64,
    /// Principal point's horizontal pixel coordinate.
    pub cx: f64,
    /// Principal point's vertical pixel coordinate.
    pub cy: f64,
}

impl Intrinsics {
    /// Builds an intrinsics matrix from its four values.
    #[must_use]
    pub const fn new(fx: f64, fy: f64, cx: f64, cy: f64) -> Self {
        Self { fx, fy, cx, cy }
    }

    /// Projects a normalised (undistorted, `z = 1`) camera-space point to a
    /// pixel coordinate: `(fx * x + cx, fy * y + cy)`.
    #[must_use]
    pub fn project_normalized(&self, normalized: (f64, f64)) -> (f64, f64) {
        (
            self.fx * normalized.0 + self.cx,
            self.fy * normalized.1 + self.cy,
        )
    }

    /// The exact inverse of [`Intrinsics::project_normalized`]: a pixel
    /// coordinate back to normalised camera-space, undoing focal length
    /// and principal point only — no distortion is modelled here.
    #[must_use]
    pub fn unproject_to_normalized(&self, pixel: (f64, f64)) -> (f64, f64) {
        ((pixel.0 - self.cx) / self.fx, (pixel.1 - self.cy) / self.fy)
    }

    /// Whether `fx`/`fy` are finite and non-zero — the one precondition
    /// [`Intrinsics::unproject_to_normalized`]'s division needs.
    fn is_valid(&self) -> bool {
        self.fx.is_finite() && self.fy.is_finite() && self.fx != 0.0 && self.fy != 0.0
    }
}

/// Plumb-bob (Brown-Conrady) lens distortion: three radial coefficients
/// (`k1`, `k2`, `k3`) and two tangential (`p1`, `p2`) — the model OpenCV's
/// `calibrateCamera`/`undistort` use, and the one most lens calibration
/// tooling reports.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Distortion {
    /// First-order radial coefficient.
    pub k1: f64,
    /// Second-order radial coefficient.
    pub k2: f64,
    /// Third-order radial coefficient.
    pub k3: f64,
    /// First tangential coefficient.
    pub p1: f64,
    /// Second tangential coefficient.
    pub p2: f64,
}

impl Distortion {
    /// No distortion at all — every coefficient zero, making
    /// [`Distortion::distort_normalized`] the identity (see
    /// [`undistort_map`]'s doc for why that is a load-bearing test
    /// invariant, not just a convenience default).
    pub const NONE: Self = Self {
        k1: 0.0,
        k2: 0.0,
        k3: 0.0,
        p1: 0.0,
        p2: 0.0,
    };

    /// Builds a distortion model from its five coefficients.
    #[must_use]
    pub const fn new(k1: f64, k2: f64, k3: f64, p1: f64, p2: f64) -> Self {
        Self { k1, k2, k3, p1, p2 }
    }

    /// Applies forward plumb-bob distortion to a normalised (undistorted)
    /// point, returning where a real lens with this model actually places
    /// it — the standard OpenCV equations:
    ///
    /// ```text
    /// r2 = x^2 + y^2
    /// radial = 1 + k1*r2 + k2*r2^2 + k3*r2^3
    /// x' = x*radial + 2*p1*x*y + p2*(r2 + 2*x^2)
    /// y' = y*radial + p1*(r2 + 2*y^2) + 2*p2*x*y
    /// ```
    ///
    /// At the origin (`x = y = 0`, i.e. `r2 = 0`) every term but the
    /// leading `1` in `radial` vanishes, so this is the identity there
    /// regardless of any coefficient's value — the principal point never
    /// moves under this model.
    #[must_use]
    pub fn distort_normalized(&self, point: (f64, f64)) -> (f64, f64) {
        let (x, y) = point;
        let r2 = x * x + y * y;
        let r4 = r2 * r2;
        let r6 = r4 * r2;
        let radial = 1.0 + self.k1 * r2 + self.k2 * r4 + self.k3 * r6;
        let x_tangential = 2.0 * self.p1 * x * y + self.p2 * (r2 + 2.0 * x * x);
        let y_tangential = self.p1 * (r2 + 2.0 * y * y) + 2.0 * self.p2 * x * y;
        (x * radial + x_tangential, y * radial + y_tangential)
    }
}

/// How [`remap`] samples between the four pixels nearest a fractional
/// source coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Interpolation {
    /// Rounds to the nearest source pixel.
    Nearest,
    /// Bilinear interpolation of the four surrounding source pixels — the
    /// default.
    #[default]
    Bilinear,
}

/// A precomputed remap lookup table: for every pixel `(u, v)` of a
/// `width`x`height` *output* frame, the fractional source coordinate
/// [`remap`] should sample — see this module's doc for how [`undistort_map`]
/// derives it.
#[derive(Debug, Clone, PartialEq)]
pub struct UndistortMap {
    width: u32,
    height: u32,
    source_x: Vec<f32>,
    source_y: Vec<f32>,
}

impl UndistortMap {
    /// The output frame's width this map was built for.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The output frame's height this map was built for.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The source coordinate output pixel `(x, y)` should sample, or
    /// [`None`] outside the map.
    #[must_use]
    pub fn source_at(&self, x: u32, y: u32) -> Option<(f32, f32)> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = (y as usize) * (self.width as usize) + (x as usize);
        Some((self.source_x[index], self.source_y[index]))
    }
}

/// Builds the `width`x`height` [`UndistortMap`] that rectifies a frame
/// captured through `distortion` under `intrinsics` (see this module's doc
/// for the three-step per-pixel derivation).
///
/// # Errors
///
/// [`VisionError::InvalidParameter`] (`what: "camera intrinsics"`) when
/// `intrinsics.fx`/`fy` is zero or not finite (the map's own construction
/// divides by both, in [`Intrinsics::unproject_to_normalized`]).
pub fn undistort_map(
    width: u32,
    height: u32,
    intrinsics: Intrinsics,
    distortion: Distortion,
) -> Result<UndistortMap> {
    if !intrinsics.is_valid() {
        return Err(VisionError::InvalidParameter {
            what: "camera intrinsics",
            reason: "fx and fy must be finite and non-zero",
        });
    }
    let mut source_x = Vec::with_capacity((width as usize) * (height as usize));
    let mut source_y = Vec::with_capacity((width as usize) * (height as usize));
    for v in 0..height {
        for u in 0..width {
            let normalized = intrinsics.unproject_to_normalized((f64::from(u), f64::from(v)));
            let distorted = distortion.distort_normalized(normalized);
            let (sx, sy) = intrinsics.project_normalized(distorted);
            source_x.push(sx as f32);
            source_y.push(sy as f32);
        }
    }
    Ok(UndistortMap {
        width,
        height,
        source_x,
        source_y,
    })
}

/// Reads `image`'s pixel nearest `(x, y)` into `dest`, leaving `dest`
/// untouched (the caller pre-zeroes it) when `(x, y)` falls outside
/// `image` — the same "outside samples as black" convention
/// `cv::remap`'s default border uses.
fn sample_nearest_into(image: &ImageBuffer, x: f32, y: f32, dest: &mut [u8]) {
    if !x.is_finite() || !y.is_finite() {
        return;
    }
    let (Ok(xi), Ok(yi)) = (
        u32::try_from(x.round() as i64),
        u32::try_from(y.round() as i64),
    ) else {
        return;
    };
    if let Some(pixel) = image.pixel(xi, yi) {
        dest.copy_from_slice(pixel);
    }
}

/// As [`sample_nearest_into`], but bilinearly interpolating the four
/// source pixels surrounding `(x, y)`; leaves `dest` untouched unless all
/// four neighbours are in range.
fn sample_bilinear_into(image: &ImageBuffer, x: f32, y: f32, dest: &mut [u8]) {
    if image.width() == 0 || image.height() == 0 {
        return;
    }
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
        return;
    }
    // Every realistic frame width/height is exact in `f32` (which
    // represents integers exactly up to 2^24); the whole-pixel subtraction
    // below never needs to worry about float rounding.
    let max_x = (image.width() - 1) as f32;
    let max_y = (image.height() - 1) as f32;
    if x > max_x || y > max_y {
        return;
    }
    let x0 = x.floor();
    let y0 = y.floor();
    let x1 = (x0 + 1.0).min(max_x);
    let y1 = (y0 + 1.0).min(max_y);
    let fx = f64::from(x - x0);
    let fy = f64::from(y - y0);
    let (x0, y0, x1, y1) = (x0 as u32, y0 as u32, x1 as u32, y1 as u32);

    let Some(top_left) = image.pixel(x0, y0) else {
        return;
    };
    let Some(top_right) = image.pixel(x1, y0) else {
        return;
    };
    let Some(bottom_left) = image.pixel(x0, y1) else {
        return;
    };
    let Some(bottom_right) = image.pixel(x1, y1) else {
        return;
    };

    for c in 0..dest.len() {
        let top = f64::from(top_left[c]) * (1.0 - fx) + f64::from(top_right[c]) * fx;
        let bottom = f64::from(bottom_left[c]) * (1.0 - fx) + f64::from(bottom_right[c]) * fx;
        dest[c] = (top * (1.0 - fy) + bottom * fy).round() as u8;
    }
}

/// Resamples `image` through `map`, producing a fresh frame of `map`'s own
/// width/height. A destination pixel whose source coordinate falls outside
/// `image` is left black (`0`).
///
/// # Errors
///
/// [`VisionError::UnsupportedFormat`] for a packed 4:2:2 source.
pub fn remap(
    image: &ImageBuffer,
    map: &UndistortMap,
    interpolation: Interpolation,
) -> Result<ImageBuffer> {
    image.require_fully_sampled("camera::remap")?;
    let bpp = image.bytes_per_pixel();
    let mut out = vec![0u8; (map.width as usize) * (map.height as usize) * bpp];
    for y in 0..map.height {
        for x in 0..map.width {
            let Some((sx, sy)) = map.source_at(x, y) else {
                continue;
            };
            let index = ((y as usize) * (map.width as usize) + (x as usize)) * bpp;
            let dest = &mut out[index..index + bpp];
            match interpolation {
                Interpolation::Nearest => sample_nearest_into(image, sx, sy, dest),
                Interpolation::Bilinear => sample_bilinear_into(image, sx, sy, dest),
            }
        }
    }
    ImageBuffer::new(image.format(), map.width, map.height, out)
}

/// Builds `image`'s own undistort map and immediately applies it — for a
/// caller with no reason to keep the map (see this module's doc: a
/// repeated-frame pipeline should call [`undistort_map`] once and
/// [`remap`] per frame instead, which is exactly what [`UndistortOperator`]
/// does).
///
/// # Errors
///
/// As [`undistort_map`] and [`remap`].
pub fn undistort(
    image: &ImageBuffer,
    intrinsics: Intrinsics,
    distortion: Distortion,
    interpolation: Interpolation,
) -> Result<ImageBuffer> {
    let map = undistort_map(image.width(), image.height(), intrinsics, distortion)?;
    remap(image, &map, interpolation)
}

/// An [`astrs_operator_api::Operator`] rectifying every `image` input
/// through a fixed camera model, publishing the result on `undistorted`.
///
/// Caches its [`UndistortMap`], rebuilding it only when an input frame's
/// size differs from the cached map's — the fixed-camera pipeline
/// [`undistort_map`]'s own doc describes, made concrete.
///
/// # Configuration
///
/// | Key | Type | Default | Meaning |
/// |---|---|---|---|
/// | `fx`, `fy`, `cx`, `cy` | float | required | Pinhole intrinsics |
/// | `k1`, `k2`, `k3`, `p1`, `p2` | float | `0.0` | Plumb-bob coefficients |
/// | `interpolation` | string | `"bilinear"` | `"nearest"` or `"bilinear"` |
#[derive(Debug, Default)]
pub struct UndistortOperator {
    intrinsics: Intrinsics,
    distortion: Distortion,
    interpolation: Interpolation,
    cached_map: Option<UndistortMap>,
}

impl astrs_operator_api::Operator for UndistortOperator {
    fn configure(
        &mut self,
        config: &std::collections::BTreeMap<String, astrs_wire::Parameter>,
    ) -> astrs_operator_api::OpResult<()> {
        let required = |key: &str| -> astrs_operator_api::OpResult<f64> {
            config
                .get(key)
                .and_then(astrs_wire::Parameter::as_float)
                .ok_or_else(|| {
                    astrs_operator_api::OpError::failed(format!(
                        "undistort requires a float {key:?}"
                    ))
                })
        };
        let optional = |key: &str| {
            config
                .get(key)
                .and_then(astrs_wire::Parameter::as_float)
                .unwrap_or(0.0)
        };

        self.intrinsics = Intrinsics::new(
            required("fx")?,
            required("fy")?,
            required("cx")?,
            required("cy")?,
        );
        self.distortion = Distortion::new(
            optional("k1"),
            optional("k2"),
            optional("k3"),
            optional("p1"),
            optional("p2"),
        );
        self.interpolation = match config
            .get("interpolation")
            .and_then(astrs_wire::Parameter::as_str)
        {
            Some("nearest") => Interpolation::Nearest,
            Some("bilinear") | None => Interpolation::Bilinear,
            Some(other) => {
                return Err(astrs_operator_api::OpError::failed(format!(
                    "undistort interpolation must be \"nearest\" or \"bilinear\", got {other:?}"
                )));
            }
        };
        // The old map (if any) was built for the previous configuration;
        // discard it so the next frame rebuilds under the new one.
        self.cached_map = None;
        Ok(())
    }

    fn on_event(
        &mut self,
        event: &astrs_operator_api::OpEvent,
        out: &mut astrs_operator_api::OpOutput,
    ) -> astrs_operator_api::OpResult<astrs_operator_api::Status> {
        match event {
            astrs_operator_api::OpEvent::Input {
                metadata, payload, ..
            } => {
                let image = crate::ops::decode_image(payload)?;
                let needs_rebuild = match &self.cached_map {
                    Some(map) => map.width() != image.width() || map.height() != image.height(),
                    None => true,
                };
                if needs_rebuild {
                    self.cached_map = Some(undistort_map(
                        image.width(),
                        image.height(),
                        self.intrinsics,
                        self.distortion,
                    )?);
                }
                // Just ensured `Some` above when it was not already; a
                // graceful continue (rather than an assumed-safe unwrap)
                // covers the structurally-unreachable `None` branch too.
                let Some(map) = self.cached_map.as_ref() else {
                    return Ok(astrs_operator_api::Status::Continue);
                };
                let result = remap(&image, map, self.interpolation)?;
                crate::ops::encode_image(out, "undistorted", metadata.clone(), &result)?;
                Ok(astrs_operator_api::Status::Continue)
            }
            astrs_operator_api::OpEvent::Stop { .. } => Ok(astrs_operator_api::Status::Finished),
            _ => Ok(astrs_operator_api::Status::Continue),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::pixel::PixelFormat;

    const INTRINSICS: Intrinsics = Intrinsics::new(500.0, 500.0, 320.0, 240.0);

    #[test]
    fn project_and_unproject_are_exact_inverses() {
        let pixel = (400.0, 300.0);
        let normalized = INTRINSICS.unproject_to_normalized(pixel);
        assert_eq!(normalized, (0.16, 0.12));
        assert_eq!(INTRINSICS.project_normalized(normalized), pixel);
    }

    #[test]
    fn distortion_is_the_identity_at_the_origin_for_any_coefficients() {
        let heavy = Distortion::new(0.5, -0.3, 0.1, 0.2, -0.1);
        assert_eq!(heavy.distort_normalized((0.0, 0.0)), (0.0, 0.0));
    }

    #[test]
    fn radial_distortion_scales_a_point_along_its_own_ray() {
        // k1 = 0.1, point (1, 0): r2 = 1, radial = 1.1, no tangential terms.
        let distortion = Distortion::new(0.1, 0.0, 0.0, 0.0, 0.0);
        let distorted = distortion.distort_normalized((1.0, 0.0));
        assert!((distorted.0 - 1.1).abs() < 1e-12);
        assert!((distorted.1 - 0.0).abs() < 1e-12);
    }

    #[test]
    fn zero_distortion_leaves_a_point_unchanged() {
        assert_eq!(
            Distortion::NONE.distort_normalized((0.3, -0.7)),
            (0.3, -0.7)
        );
    }

    #[test]
    fn a_zero_distortion_map_is_the_identity_everywhere() {
        let map = undistort_map(8, 6, INTRINSICS, Distortion::NONE).unwrap();
        for y in 0..6 {
            for x in 0..8 {
                let (sx, sy) = map.source_at(x, y).unwrap();
                assert!((sx - x as f32).abs() < 1e-3, "x mismatch at ({x},{y})");
                assert!((sy - y as f32).abs() < 1e-3, "y mismatch at ({x},{y})");
            }
        }
    }

    #[test]
    fn the_principal_point_never_moves_under_any_distortion() {
        // cx/cy chosen as exact integer pixel coordinates of a small frame.
        let intrinsics = Intrinsics::new(100.0, 100.0, 2.0, 2.0);
        let heavy = Distortion::new(0.8, -0.4, 0.2, 0.3, -0.2);
        let map = undistort_map(5, 5, intrinsics, heavy).unwrap();
        let (sx, sy) = map.source_at(2, 2).unwrap();
        assert!((sx - 2.0).abs() < 1e-4);
        assert!((sy - 2.0).abs() < 1e-4);
    }

    #[test]
    fn undistort_map_rejects_a_zero_focal_length() {
        let bad = Intrinsics::new(0.0, 500.0, 320.0, 240.0);
        assert!(matches!(
            undistort_map(4, 4, bad, Distortion::NONE),
            Err(VisionError::InvalidParameter {
                what: "camera intrinsics",
                ..
            })
        ));
    }

    #[test]
    fn remapping_through_an_identity_map_reproduces_the_source() {
        let image = ImageBuffer::new(PixelFormat::Mono8, 4, 3, (0..12u8).collect()).unwrap();
        let map = undistort_map(4, 3, INTRINSICS, Distortion::NONE).unwrap();
        for interpolation in [Interpolation::Nearest, Interpolation::Bilinear] {
            let remapped = remap(&image, &map, interpolation).unwrap();
            assert_eq!(remapped, image, "{interpolation:?}");
        }
    }

    #[test]
    fn remap_leaves_an_out_of_range_source_black() {
        let image = ImageBuffer::new(PixelFormat::Mono8, 2, 2, vec![10, 20, 30, 40]).unwrap();
        // A map that (for its one output pixel) points far outside the
        // 2x2 source.
        let map = UndistortMap {
            width: 1,
            height: 1,
            source_x: vec![50.0],
            source_y: vec![50.0],
        };
        let remapped = remap(&image, &map, Interpolation::Bilinear).unwrap();
        assert_eq!(remapped.data(), &[0]);
    }

    #[test]
    fn undistort_end_to_end_matches_a_map_then_remap() {
        let image = ImageBuffer::new(PixelFormat::Mono8, 5, 5, (0..25u8).collect()).unwrap();
        let distortion = Distortion::new(0.05, 0.0, 0.0, 0.0, 0.0);
        let intrinsics = Intrinsics::new(50.0, 50.0, 2.0, 2.0);
        let via_convenience =
            undistort(&image, intrinsics, distortion, Interpolation::Nearest).unwrap();
        let map = undistort_map(5, 5, intrinsics, distortion).unwrap();
        let via_parts = remap(&image, &map, Interpolation::Nearest).unwrap();
        assert_eq!(via_convenience, via_parts);
    }

    // ---- Operators ----
    //
    // `UndistortOperator` is the one operator in this crate with any
    // instance state of its own (its cached `UndistortMap`, rebuilt only
    // when an input frame's size differs from the cached map's -- see its
    // own doc). A test that only ever sends one frame size can never
    // exercise that rebuild branch, so this sends two different sizes
    // through one operator instance.

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

    fn image_payload(image: &ImageBuffer) -> Vec<u8> {
        let batch = image.to_message().unwrap().to_record_batch().unwrap();
        astrs_data::ipc::encode_payload(&batch).unwrap().to_vec()
    }

    fn decode_image_payload(payload: &[u8]) -> ImageBuffer {
        let batch = astrs_data::ipc::decode_payload(payload).unwrap();
        let wire_image = astrs_node_api::message::Image::from_record_batch(&batch).unwrap();
        ImageBuffer::from_message(&wire_image).unwrap()
    }

    #[test]
    fn undistort_operator_rebuilds_its_cached_map_when_the_frame_size_changes() {
        use astrs_operator_api::{OpOutput, Operator, Status};

        let intrinsics = Intrinsics::new(50.0, 50.0, 2.0, 2.0);
        let distortion = Distortion::new(0.05, 0.0, 0.0, 0.0, 0.0);

        let mut op = UndistortOperator::default();
        let mut config = std::collections::BTreeMap::new();
        for (key, value) in [
            ("fx", intrinsics.fx),
            ("fy", intrinsics.fy),
            ("cx", intrinsics.cx),
            ("cy", intrinsics.cy),
            ("k1", distortion.k1),
        ] {
            config.insert(key.to_owned(), astrs_wire::Parameter::Float(value));
        }
        op.configure(&config).unwrap();
        let mut out = OpOutput::new();

        // First frame: 5x5 -- the operator's cache starts empty, so this
        // forces the first build.
        let small = ImageBuffer::new(PixelFormat::Mono8, 5, 5, (0..25u8).collect()).unwrap();
        let status = op
            .on_event(&input_event(image_payload(&small)), &mut out)
            .unwrap();
        assert_eq!(status, Status::Continue);
        let sends = out.drain();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].id().as_str(), "undistorted");
        let published_small = decode_image_payload(sends[0].payload());
        let expected_small =
            undistort(&small, intrinsics, distortion, Interpolation::Bilinear).unwrap();
        assert_eq!(published_small, expected_small);

        // Second frame: a different size (8x6). `remap` always sizes its
        // output to the *map's own* width/height (see `remap`'s doc), so a
        // stale 5x5 map that was never rebuilt would silently publish a
        // wrong-sized 5x5 frame here instead of the correct 8x6 one --
        // exactly the bug this test exists to catch.
        let large = ImageBuffer::new(PixelFormat::Mono8, 8, 6, (0..48u8).collect()).unwrap();
        let status = op
            .on_event(&input_event(image_payload(&large)), &mut out)
            .unwrap();
        assert_eq!(status, Status::Continue);
        let sends = out.drain();
        assert_eq!(sends.len(), 1);
        let published_large = decode_image_payload(sends[0].payload());
        assert_eq!(
            (published_large.width(), published_large.height()),
            (8, 6),
            "the cached map must have rebuilt for the new frame size"
        );
        let expected_large =
            undistort(&large, intrinsics, distortion, Interpolation::Bilinear).unwrap();
        assert_eq!(published_large, expected_large);
    }

    #[test]
    fn undistort_operator_finishes_on_stop() {
        use astrs_operator_api::{OpEvent, OpOutput, Operator, Status};

        let mut op = UndistortOperator::default();
        let mut out = OpOutput::new();
        let stop = OpEvent::Stop {
            cause: astrs_wire::StopCause::Requested,
            grace: None,
        };
        assert_eq!(op.on_event(&stop, &mut out).unwrap(), Status::Finished);
        assert!(out.is_empty());
    }
}
