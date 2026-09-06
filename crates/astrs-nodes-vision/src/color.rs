//! Colour-space and pixel-format conversion.
//!
//! Every function here reads one [`ImageBuffer`] and returns a fresh one —
//! never mutates in place, since a conversion frequently changes the byte
//! layout (and, for YUV422, the geometry it can neighbour). [`convert`] is
//! the one entry point that reaches any [`PixelFormat`] from any other; the
//! named functions ([`to_gray`], [`to_rgb8`], [`yuv422_to_rgb8`], ...) are
//! the direct, single-purpose routes `convert` itself is built from, exposed
//! because a caller who already knows the source and target format wants to
//! name the conversion, not describe it as a fold over a dispatcher.
//!
//! # RGB8 is the pivot
//!
//! [`convert`] normalises through [`PixelFormat::Rgb8`]: it calls
//! [`to_rgb8`] on the input, then goes from that pivot to the requested
//! target. This costs one intermediate allocation for a conversion that
//! is not already `X -> Rgb8` or `Rgb8 -> X` (e.g. `Bgr8 -> Yuyv`), in
//! exchange for needing exactly `2 * (N - 1)` conversion routines for `N`
//! formats instead of `N * (N - 1)` — and every route is still one row-wise
//! pass with no per-pixel allocation.
//!
//! # YUV422: full-range BT.601 (JFIF), not studio-range
//!
//! [`PixelFormat::Yuyv`]/[`PixelFormat::Uyvy`] carry `Y`/`Cb`/`Cr` using the
//! full `0..=255` range for all three channels (the JFIF/JPEG convention),
//! not the studio-range `16..=235`/`16..=240` MPEG convention — this is what
//! `libv4l`/`ffmpeg`'s default `bt601` (as opposed to `bt601-full`... the
//! naming is famously inconsistent across tools) assumes for a UVC webcam,
//! and it is the simpler, more common choice for a machine-vision pipeline
//! that has no reason to preserve broadcast headroom. All the arithmetic is
//! 16-bit fixed-point (`>> 16` in place of `/ 65536.0`), which is exact
//! enough for 8-bit samples and keeps every pixel op in this crate
//! integer-only.
//!
//! A round trip through [`PixelFormat::Yuyv`]/[`PixelFormat::Uyvy`] is
//! lossy by construction — two horizontally-adjacent pixels share one
//! chroma sample — so `rgb8_to_yuyv` then `yuv422_to_rgb8` does not
//! reproduce the original exactly except where neighbouring pixels already
//! agreed on colour (see this module's tests for the exact cases that do).

use crate::buffer::ImageBuffer;
use crate::error::{Result, VisionError};
use crate::pixel::PixelFormat;

/// Converts `image` to `target`, choosing whichever of this module's direct
/// routes apply (through the [`PixelFormat::Rgb8`] pivot when neither format
/// is already the source or the target of a direct one).
///
/// Returns a clone, not an error, when `image.format() == target`.
///
/// # Errors
///
/// Whatever the chosen route reports — in practice only
/// [`VisionError::InvalidGeometry`], and only when `target` is
/// [`PixelFormat::Yuyv`]/[`PixelFormat::Uyvy`] and `image.width()` is odd.
pub fn convert(image: &ImageBuffer, target: PixelFormat) -> Result<ImageBuffer> {
    if image.format() == target {
        return Ok(image.clone());
    }
    match target {
        PixelFormat::Mono8 => to_gray(image),
        PixelFormat::Rgb8 => to_rgb8(image),
        PixelFormat::Bgr8 => to_bgr8(image),
        PixelFormat::Rgba8 => to_rgba8(image, DEFAULT_ALPHA),
        PixelFormat::Yuyv => rgb8_to_yuv422(&to_rgb8(image)?, Yuv422Order::Yuyv),
        PixelFormat::Uyvy => rgb8_to_yuv422(&to_rgb8(image)?, Yuv422Order::Uyvy),
    }
}

/// The alpha value [`convert`] and [`to_rgba8`] fill in for a source format
/// that has no alpha channel of its own: fully opaque.
pub const DEFAULT_ALPHA: u8 = 255;

/// Converts to [`PixelFormat::Mono8`] (ITU-R BT.601 luma, `L = 0.299R +
/// 0.587G + 0.114B`, rounded).
///
/// For [`PixelFormat::Yuyv`]/[`PixelFormat::Uyvy`] this reads the `Y` samples
/// directly rather than decoding to RGB first — exact, and cheaper, since
/// luma is already what those formats store.
///
/// # Errors
///
/// Never, for any format this crate has — kept `Result` for symmetry with
/// every other function in this module and to absorb a future format this
/// cannot yet reach without an API break.
pub fn to_gray(image: &ImageBuffer) -> Result<ImageBuffer> {
    match image.format() {
        PixelFormat::Mono8 => Ok(image.clone()),
        PixelFormat::Rgb8 | PixelFormat::Rgba8 => gray_from_channels(image, 0, 1, 2),
        PixelFormat::Bgr8 => gray_from_channels(image, 2, 1, 0),
        PixelFormat::Yuyv | PixelFormat::Uyvy => extract_luma_plane(image),
    }
}

/// Converts to [`PixelFormat::Rgb8`].
///
/// # Errors
///
/// [`VisionError::InvalidGeometry`]: never actually reachable for a valid
/// `image` (every source format this crate has already has a valid `Rgb8`
/// frame at the same width/height) — `Result` is kept for symmetry with the
/// rest of this module.
pub fn to_rgb8(image: &ImageBuffer) -> Result<ImageBuffer> {
    match image.format() {
        PixelFormat::Rgb8 => Ok(image.clone()),
        PixelFormat::Mono8 => gray_to_channels(image, PixelFormat::Rgb8, DEFAULT_ALPHA),
        PixelFormat::Bgr8 => swap_red_blue(image, PixelFormat::Rgb8),
        PixelFormat::Rgba8 => drop_alpha(image),
        PixelFormat::Yuyv => yuv422_to_regular(image, Yuv422Order::Yuyv, PixelFormat::Rgb8),
        PixelFormat::Uyvy => yuv422_to_regular(image, Yuv422Order::Uyvy, PixelFormat::Rgb8),
    }
}

/// Converts to [`PixelFormat::Bgr8`].
///
/// # Errors
///
/// As [`to_rgb8`].
pub fn to_bgr8(image: &ImageBuffer) -> Result<ImageBuffer> {
    match image.format() {
        PixelFormat::Bgr8 => Ok(image.clone()),
        PixelFormat::Mono8 => gray_to_channels(image, PixelFormat::Bgr8, DEFAULT_ALPHA),
        PixelFormat::Rgb8 => swap_red_blue(image, PixelFormat::Bgr8),
        PixelFormat::Rgba8 => swap_red_blue(&drop_alpha(image)?, PixelFormat::Bgr8),
        PixelFormat::Yuyv => yuv422_to_regular(image, Yuv422Order::Yuyv, PixelFormat::Bgr8),
        PixelFormat::Uyvy => yuv422_to_regular(image, Yuv422Order::Uyvy, PixelFormat::Bgr8),
    }
}

/// Converts to [`PixelFormat::Rgba8`], filling in `alpha` for a source
/// format with no alpha channel of its own.
///
/// # Errors
///
/// As [`to_rgb8`].
pub fn to_rgba8(image: &ImageBuffer, alpha: u8) -> Result<ImageBuffer> {
    match image.format() {
        PixelFormat::Rgba8 => Ok(image.clone()),
        PixelFormat::Mono8 => gray_to_channels(image, PixelFormat::Rgba8, alpha),
        _ => add_alpha(&to_rgb8(image)?, alpha),
    }
}

/// Decodes a packed 4:2:2 YUV frame to [`PixelFormat::Rgb8`].
///
/// # Errors
///
/// [`VisionError::UnsupportedFormat`] when `image.format()` is neither
/// [`PixelFormat::Yuyv`] nor [`PixelFormat::Uyvy`].
pub fn yuv422_to_rgb8(image: &ImageBuffer) -> Result<ImageBuffer> {
    let order = yuv422_order(image, "color::yuv422_to_rgb8")?;
    yuv422_to_regular(image, order, PixelFormat::Rgb8)
}

/// Decodes a packed 4:2:2 YUV frame to [`PixelFormat::Bgr8`].
///
/// # Errors
///
/// As [`yuv422_to_rgb8`].
pub fn yuv422_to_bgr8(image: &ImageBuffer) -> Result<ImageBuffer> {
    let order = yuv422_order(image, "color::yuv422_to_bgr8")?;
    yuv422_to_regular(image, order, PixelFormat::Bgr8)
}

/// Encodes an [`PixelFormat::Rgb8`] frame as packed [`PixelFormat::Yuyv`],
/// averaging each horizontally-adjacent pixel pair's chroma.
///
/// # Errors
///
/// [`VisionError::UnsupportedFormat`] when `image.format()` is not
/// [`PixelFormat::Rgb8`]; [`VisionError::InvalidGeometry`] when
/// `image.width()` is odd (4:2:2 packing needs a chroma-sharing pair for
/// every pixel).
pub fn rgb8_to_yuyv(image: &ImageBuffer) -> Result<ImageBuffer> {
    require_rgb8(image, "color::rgb8_to_yuyv")?;
    rgb8_to_yuv422(image, Yuv422Order::Yuyv)
}

/// Encodes an [`PixelFormat::Rgb8`] frame as packed [`PixelFormat::Uyvy`].
///
/// # Errors
///
/// As [`rgb8_to_yuyv`].
pub fn rgb8_to_uyvy(image: &ImageBuffer) -> Result<ImageBuffer> {
    require_rgb8(image, "color::rgb8_to_uyvy")?;
    rgb8_to_yuv422(image, Yuv422Order::Uyvy)
}

/// The two packed-4:2:2 byte orders, and where each puts `Y0`/`U`/`Y1`/`V`
/// within its four-byte group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Yuv422Order {
    Yuyv,
    Uyvy,
}

impl Yuv422Order {
    /// The order a format uses, or [`None`] for a format that is not
    /// packed 4:2:2 at all.
    const fn of(format: PixelFormat) -> Option<Self> {
        match format {
            PixelFormat::Yuyv => Some(Self::Yuyv),
            PixelFormat::Uyvy => Some(Self::Uyvy),
            _ => None,
        }
    }

    /// The [`PixelFormat`] this order belongs to.
    const fn format(self) -> PixelFormat {
        match self {
            Self::Yuyv => PixelFormat::Yuyv,
            Self::Uyvy => PixelFormat::Uyvy,
        }
    }

    /// `(y0, u, y1, v)`: the byte offset of each sample within one
    /// four-byte group.
    const fn layout(self) -> (usize, usize, usize, usize) {
        match self {
            Self::Yuyv => (0, 1, 2, 3),
            Self::Uyvy => (1, 0, 3, 2),
        }
    }
}

/// `image`'s [`Yuv422Order`], or [`VisionError::UnsupportedFormat`] naming
/// `op` when it has none.
fn yuv422_order(image: &ImageBuffer, op: &'static str) -> Result<Yuv422Order> {
    Yuv422Order::of(image.format()).ok_or(VisionError::UnsupportedFormat {
        op,
        format: image.format(),
    })
}

/// [`VisionError::UnsupportedFormat`] naming `op` unless `image` is already
/// [`PixelFormat::Rgb8`].
fn require_rgb8(image: &ImageBuffer, op: &'static str) -> Result<()> {
    if image.format() == PixelFormat::Rgb8 {
        Ok(())
    } else {
        Err(VisionError::UnsupportedFormat {
            op,
            format: image.format(),
        })
    }
}

/// Decodes every 4:2:2 group in `image` (already known to use `order`) to
/// `target`, which must be [`PixelFormat::Rgb8`] or [`PixelFormat::Bgr8`].
fn yuv422_to_regular(
    image: &ImageBuffer,
    order: Yuv422Order,
    target: PixelFormat,
) -> Result<ImageBuffer> {
    let (y0i, ui, y1i, vi) = order.layout();
    let capacity = (image.width() as usize) * (image.height() as usize) * target.bytes_per_pixel();
    let mut out = Vec::with_capacity(capacity);
    for group in image.data().as_chunks::<4>().0 {
        let (y0, u, y1, v) = (group[y0i], group[ui], group[y1i], group[vi]);
        let (r0, g0, b0) = ycbcr_to_rgb(y0, u, v);
        let (r1, g1, b1) = ycbcr_to_rgb(y1, u, v);
        push_pixel(&mut out, target, r0, g0, b0);
        push_pixel(&mut out, target, r1, g1, b1);
    }
    ImageBuffer::new(target, image.width(), image.height(), out)
}

/// Encodes `image` (already known to be `Rgb8`) as packed 4:2:2 in `order`,
/// averaging each adjacent pixel pair's chroma.
fn rgb8_to_yuv422(image: &ImageBuffer, order: Yuv422Order) -> Result<ImageBuffer> {
    let width = image.width() as usize;
    if !width.is_multiple_of(2) {
        return Err(VisionError::InvalidGeometry {
            format: order.format(),
            width: image.width(),
            height: image.height(),
            reason: "packed 4:2:2 encoding needs an even width",
        });
    }
    let (y0i, ui, y1i, vi) = order.layout();
    let mut out = vec![0u8; width * (image.height() as usize) * 2];
    // `zip` stops at whichever side runs out, which is exactly the bound
    // the explicit cursor-and-`break` pair enforced: a frame carrying less
    // data than its declared geometry leaves the tail of `out` zeroed
    // rather than reading past either slice.
    for (group, &[r0, g0, b0, r1, g1, b1]) in out
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(image.data().as_chunks::<6>().0)
    {
        group[y0i] = rgb_to_y(r0, g0, b0);
        group[y1i] = rgb_to_y(r1, g1, b1);
        group[ui] = average_u8(rgb_to_cb(r0, g0, b0), rgb_to_cb(r1, g1, b1));
        group[vi] = average_u8(rgb_to_cr(r0, g0, b0), rgb_to_cr(r1, g1, b1));
    }
    ImageBuffer::new(order.format(), image.width(), image.height(), out)
}

/// Appends one pixel's bytes to `out` in `target`'s channel order.
///
/// `target` must be [`PixelFormat::Rgb8`] or [`PixelFormat::Bgr8`]; every
/// other format falls back to RGB order (unreachable in practice — both of
/// this function's call sites only ever pass one of the two).
fn push_pixel(out: &mut Vec<u8>, target: PixelFormat, r: u8, g: u8, b: u8) {
    if target == PixelFormat::Bgr8 {
        out.push(b);
        out.push(g);
        out.push(r);
    } else {
        out.push(r);
        out.push(g);
        out.push(b);
    }
}

/// Reads the `Y` sample of every pixel in a packed 4:2:2 frame directly,
/// without decoding chroma.
fn extract_luma_plane(image: &ImageBuffer) -> Result<ImageBuffer> {
    let order = yuv422_order(image, "color::to_gray")?;
    let (y0i, _, y1i, _) = order.layout();
    let mut out = Vec::with_capacity((image.width() as usize) * (image.height() as usize));
    for group in image.data().as_chunks::<4>().0 {
        out.push(group[y0i]);
        out.push(group[y1i]);
    }
    ImageBuffer::new(PixelFormat::Mono8, image.width(), image.height(), out)
}

/// Swaps the first and third byte of every pixel — `Rgb8 <-> Bgr8`.
fn swap_red_blue(image: &ImageBuffer, target: PixelFormat) -> Result<ImageBuffer> {
    let mut out = image.data().to_vec();
    for pixel in out.as_chunks_mut::<3>().0 {
        pixel.swap(0, 2);
    }
    ImageBuffer::new(target, image.width(), image.height(), out)
}

/// Drops the fourth byte of every pixel — `Rgba8 -> Rgb8`.
fn drop_alpha(image: &ImageBuffer) -> Result<ImageBuffer> {
    let mut out = Vec::with_capacity(image.data().len() / 4 * 3);
    for pixel in image.data().as_chunks::<4>().0 {
        out.extend_from_slice(&pixel[..3]);
    }
    ImageBuffer::new(PixelFormat::Rgb8, image.width(), image.height(), out)
}

/// Appends a fixed `alpha` byte to every pixel — `Rgb8 -> Rgba8`.
fn add_alpha(image: &ImageBuffer, alpha: u8) -> Result<ImageBuffer> {
    let mut out = Vec::with_capacity(image.data().len() / 3 * 4);
    for pixel in image.data().as_chunks::<3>().0 {
        out.extend_from_slice(pixel);
        out.push(alpha);
    }
    ImageBuffer::new(PixelFormat::Rgba8, image.width(), image.height(), out)
}

/// Replicates each grey sample across the first three bytes of `target`,
/// appending `alpha` when `target` has a fourth. `target` must be
/// [`PixelFormat::Rgb8`], [`PixelFormat::Bgr8`], or [`PixelFormat::Rgba8`]
/// (grey replicated across R/G/B is byte-identical whichever of the first
/// two the caller asked for).
fn gray_to_channels(image: &ImageBuffer, target: PixelFormat, alpha: u8) -> Result<ImageBuffer> {
    let has_alpha = target.has_alpha();
    let per_pixel = if has_alpha { 4 } else { 3 };
    let mut out = Vec::with_capacity(image.data().len() * per_pixel);
    for &grey in image.data() {
        out.push(grey);
        out.push(grey);
        out.push(grey);
        if has_alpha {
            out.push(alpha);
        }
    }
    ImageBuffer::new(target, image.width(), image.height(), out)
}

/// Computes [`PixelFormat::Mono8`] luma from three channels at byte offsets
/// `(red_at, green_at, blue_at)` within each pixel of `image`.
fn gray_from_channels(
    image: &ImageBuffer,
    red_at: usize,
    green_at: usize,
    blue_at: usize,
) -> Result<ImageBuffer> {
    let bpp = image.bytes_per_pixel();
    let mut out = Vec::with_capacity((image.width() as usize) * (image.height() as usize));
    for pixel in image.data().chunks_exact(bpp) {
        out.push(luma(pixel[red_at], pixel[green_at], pixel[blue_at]));
    }
    ImageBuffer::new(PixelFormat::Mono8, image.width(), image.height(), out)
}

/// ITU-R BT.601 luma, `L = 0.299R + 0.587G + 0.114B`, rounded to the nearest
/// integer. Exact for a grey input (`luma(k, k, k) == k` for every `k`):
/// the three weights sum to exactly `1000`, so `(1000k + 500) / 1000`
/// truncates back to `k`.
fn luma(r: u8, g: u8, b: u8) -> u8 {
    let value = 299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b) + 500;
    // `value <= 1000 * 255 + 500 = 255_500`, so `value / 1000 <= 255`.
    (value / 1000) as u8
}

/// Full-range BT.601 `YCbCr -> RGB` (see this module's doc for the range
/// convention), 16-bit fixed point.
fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8) -> (u8, u8, u8) {
    let y = i32::from(y);
    let d = i32::from(cb) - 128;
    let e = i32::from(cr) - 128;
    let r = y + ((91_881 * e) >> 16);
    let g = y - ((22_554 * d + 46_802 * e) >> 16);
    let b = y + ((116_130 * d) >> 16);
    (clamp_u8(r), clamp_u8(g), clamp_u8(b))
}

/// Full-range BT.601 `R,G,B -> Y`, 16-bit fixed point. The three weights sum
/// to exactly `65536`, so a grey pixel maps to `Y == grey` exactly.
fn rgb_to_y(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    clamp_u8((19_595 * r + 38_470 * g + 7_471 * b + 32_768) >> 16)
}

/// Full-range BT.601 `R,G,B -> Cb`, 16-bit fixed point. The three weights
/// sum to exactly `0`, so a grey pixel maps to `Cb == 128` exactly.
fn rgb_to_cb(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    clamp_u8(((-11_058 * r - 21_710 * g + 32_768 * b) >> 16) + 128)
}

/// Full-range BT.601 `R,G,B -> Cr`, 16-bit fixed point. The three weights
/// sum to exactly `0`, so a grey pixel maps to `Cr == 128` exactly.
fn rgb_to_cr(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    clamp_u8(((32_768 * r - 27_439 * g - 5_329 * b) >> 16) + 128)
}

/// The rounded average of two bytes.
fn average_u8(a: u8, b: u8) -> u8 {
    (u16::from(a) + u16::from(b)).div_ceil(2) as u8
}

/// Clamps a signed intermediate colour value into `u8` range.
fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// An [`astrs_operator_api::Operator`] applying [`convert`] to every `image`
/// input, publishing the converted frame on `converted`.
///
/// # Configuration
///
/// | Key | Type | Default | Meaning |
/// |---|---|---|---|
/// | `target` | string | required | One of [`PixelFormat::as_str`]'s spellings, other than `"yuyv"`/`"uyvy"` (a converted frame this operator cannot then publish — see [`crate::buffer::ImageBuffer::to_message`]) |
#[derive(Debug, Default)]
pub struct ColorConvertOperator {
    target: Option<PixelFormat>,
}

impl astrs_operator_api::Operator for ColorConvertOperator {
    fn configure(
        &mut self,
        config: &std::collections::BTreeMap<String, astrs_wire::Parameter>,
    ) -> astrs_operator_api::OpResult<()> {
        let name = config
            .get("target")
            .and_then(astrs_wire::Parameter::as_str)
            .ok_or_else(|| {
                astrs_operator_api::OpError::failed("color-convert requires a string \"target\"")
            })?;
        let format = PixelFormat::parse(name).ok_or_else(|| {
            astrs_operator_api::OpError::failed(format!(
                "color-convert target {name:?} is not a known pixel format"
            ))
        })?;
        if matches!(format, PixelFormat::Yuyv | PixelFormat::Uyvy) {
            return Err(astrs_operator_api::OpError::failed(format!(
                "color-convert target {name:?} has no wire representation to publish; \
                 convert to a fully-sampled format instead"
            )));
        }
        self.target = Some(format);
        Ok(())
    }

    fn on_event(
        &mut self,
        event: &astrs_operator_api::OpEvent,
        out: &mut astrs_operator_api::OpOutput,
    ) -> astrs_operator_api::OpResult<astrs_operator_api::Status> {
        let Some(target) = self.target else {
            return Err(astrs_operator_api::OpError::failed(
                "color-convert operator used before configure() set a target",
            ));
        };
        crate::ops::forward_image(event, out, "converted", |image| convert(image, target))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use proptest::prelude::*;

    fn mono8(pixels: &[u8]) -> ImageBuffer {
        ImageBuffer::new(PixelFormat::Mono8, pixels.len() as u32, 1, pixels.to_vec()).unwrap()
    }

    fn rgb8(pixels: &[[u8; 3]]) -> ImageBuffer {
        let data = pixels.iter().flatten().copied().collect();
        ImageBuffer::new(PixelFormat::Rgb8, pixels.len() as u32, 1, data).unwrap()
    }

    #[test]
    fn luma_of_a_grey_pixel_is_exact_for_every_level() {
        for level in 0..=255u8 {
            assert_eq!(luma(level, level, level), level, "level {level}");
        }
    }

    #[test]
    fn mono8_through_rgb8_and_back_is_lossless() {
        let grey = mono8(&[0, 1, 127, 128, 254, 255]);
        let rgb = to_rgb8(&grey).unwrap();
        assert_eq!(rgb.format(), PixelFormat::Rgb8);
        assert_eq!(
            rgb.data(),
            &[
                0, 0, 0, 1, 1, 1, 127, 127, 127, 128, 128, 128, 254, 254, 254, 255, 255, 255
            ]
        );
        assert_eq!(to_gray(&rgb).unwrap(), grey);
    }

    #[test]
    fn rgb8_and_bgr8_round_trip_by_swapping_red_and_blue() {
        let rgb = rgb8(&[[10, 20, 30], [200, 100, 50]]);
        let bgr = to_bgr8(&rgb).unwrap();
        assert_eq!(bgr.data(), &[30, 20, 10, 50, 100, 200]);
        assert_eq!(to_rgb8(&bgr).unwrap(), rgb);
    }

    #[test]
    fn rgba8_conversion_carries_channels_and_fills_the_requested_alpha() {
        let rgb = rgb8(&[[1, 2, 3]]);
        let rgba = to_rgba8(&rgb, 42).unwrap();
        assert_eq!(rgba.data(), &[1, 2, 3, 42]);
        assert_eq!(to_rgb8(&rgba).unwrap(), rgb);
        assert_eq!(to_gray(&rgba).unwrap().data(), &[luma(1, 2, 3)]);
    }

    #[test]
    fn convert_to_the_same_format_clones_without_touching_the_bytes() {
        let rgb = rgb8(&[[9, 8, 7]]);
        assert_eq!(convert(&rgb, PixelFormat::Rgb8).unwrap(), rgb);
    }

    #[test]
    fn a_grey_pixel_encodes_to_neutral_chroma_exactly() {
        for level in [0u8, 1, 127, 200, 255] {
            assert_eq!(rgb_to_y(level, level, level), level, "level {level}");
            assert_eq!(rgb_to_cb(level, level, level), 128, "level {level}");
            assert_eq!(rgb_to_cr(level, level, level), 128, "level {level}");
        }
    }

    #[test]
    fn yuyv_and_uyvy_decode_the_same_colours_from_the_same_samples() {
        // Y0=200 U=90 Y1=60 V=180, just placed at each order's own offsets.
        let (y0, u, y1, v) = (200u8, 90u8, 60u8, 180u8);
        let yuyv = ImageBuffer::new(PixelFormat::Yuyv, 2, 1, vec![y0, u, y1, v]).unwrap();
        let uyvy = ImageBuffer::new(PixelFormat::Uyvy, 2, 1, vec![u, y0, v, y1]).unwrap();
        assert_eq!(
            yuv422_to_rgb8(&yuyv).unwrap(),
            yuv422_to_rgb8(&uyvy).unwrap()
        );
    }

    #[test]
    fn a_solid_colour_survives_a_yuyv_round_trip_within_fixed_point_rounding() {
        // Two adjacent pixels of the *same* colour: chroma subsampling
        // introduces no loss (averaging two equal values reproduces them),
        // so only 16-bit fixed-point rounding can move a channel, by at
        // most a couple of levels.
        for color in [
            [255u8, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [255, 255, 255],
            [17, 200, 64],
        ] {
            let source = rgb8(&[color, color]);
            let yuyv = rgb8_to_yuyv(&source).unwrap();
            assert_eq!(yuyv.format(), PixelFormat::Yuyv);
            let recovered = yuv422_to_rgb8(&yuyv).unwrap();
            for (channel, (&want, &got)) in color.iter().zip(recovered.data().iter()).enumerate() {
                let delta = i32::from(want).abs_diff(i32::from(got));
                assert!(
                    delta <= 3,
                    "channel {channel} of {color:?}: wanted {want}, got {got}"
                );
            }
        }
    }

    #[test]
    fn yuv422_encode_rejects_an_odd_width() {
        let source = rgb8(&[[1, 2, 3]]);
        assert!(matches!(
            rgb8_to_yuyv(&source),
            Err(VisionError::InvalidGeometry { .. })
        ));
    }

    #[test]
    fn yuv422_encode_requires_an_rgb8_source() {
        let bgr = ImageBuffer::new(PixelFormat::Bgr8, 2, 1, vec![0; 6]).unwrap();
        assert!(matches!(
            rgb8_to_yuyv(&bgr),
            Err(VisionError::UnsupportedFormat { .. })
        ));
    }

    #[test]
    fn yuv422_decode_requires_a_yuv422_source() {
        let rgb = rgb8(&[[1, 2, 3]]);
        assert!(matches!(
            yuv422_to_rgb8(&rgb),
            Err(VisionError::UnsupportedFormat { .. })
        ));
    }

    proptest! {
        #[test]
        fn rgb8_bgr8_round_trip_is_exact_for_any_bytes(
            bytes in proptest::collection::vec(any::<u8>(), 3..=30)
                .prop_map(|mut v| { v.truncate(v.len() - v.len() % 3); v })
                .prop_filter("need at least one pixel", |v| !v.is_empty())
        ) {
            let width = (bytes.len() / 3) as u32;
            let rgb = ImageBuffer::new(PixelFormat::Rgb8, width, 1, bytes).unwrap();
            let bgr = to_bgr8(&rgb).unwrap();
            let back = to_rgb8(&bgr).unwrap();
            prop_assert_eq!(back, rgb);
        }

        #[test]
        fn mono8_round_trips_through_every_regular_format(level in any::<u8>()) {
            let grey = mono8(&[level]);
            for target in [PixelFormat::Rgb8, PixelFormat::Bgr8, PixelFormat::Rgba8] {
                let converted = convert(&grey, target).unwrap();
                prop_assert_eq!(to_gray(&converted).unwrap(), grey.clone());
            }
        }
    }

    fn config_of(
        pairs: &[(&str, astrs_wire::Parameter)],
    ) -> std::collections::BTreeMap<String, astrs_wire::Parameter> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn color_convert_operator_accepts_a_known_target() {
        use astrs_operator_api::Operator;
        let mut op = ColorConvertOperator::default();
        op.configure(&config_of(&[(
            "target",
            astrs_wire::Parameter::String("bgr8".to_owned()),
        )]))
        .unwrap();
        assert_eq!(op.target, Some(PixelFormat::Bgr8));
    }

    #[test]
    fn color_convert_operator_rejects_an_unknown_target_name() {
        use astrs_operator_api::Operator;
        let mut op = ColorConvertOperator::default();
        let error = op
            .configure(&config_of(&[(
                "target",
                astrs_wire::Parameter::String("cmyk".to_owned()),
            )]))
            .unwrap_err();
        assert!(matches!(error, astrs_operator_api::OpError::Failed { .. }));
    }

    #[test]
    fn color_convert_operator_rejects_a_packed_target_up_front() {
        use astrs_operator_api::Operator;
        let mut op = ColorConvertOperator::default();
        let error = op
            .configure(&config_of(&[(
                "target",
                astrs_wire::Parameter::String("yuyv".to_owned()),
            )]))
            .unwrap_err();
        assert!(matches!(error, astrs_operator_api::OpError::Failed { .. }));
        assert_eq!(op.target, None, "the rejected target must never be applied");
    }

    // ---- Operators (end to end) ----

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

    #[test]
    fn color_convert_operator_used_before_configure_reports_a_clear_error() {
        // `self.target` is only ever set by `configure` (see its own doc:
        // "no wire representation to publish" is the *rejected*-input
        // error; this is the distinct *never-configured* one) -- a host
        // that skipped configuration entirely must not panic or silently
        // pass the frame through, it must fail the event with a message
        // that says why.
        use astrs_operator_api::{OpOutput, Operator};

        let image = rgb8(&[[1, 2, 3]]);
        let mut op = ColorConvertOperator::default();
        let mut out = OpOutput::new();
        let error = op
            .on_event(&input_event(image_payload(&image)), &mut out)
            .unwrap_err();
        assert!(matches!(
            error,
            astrs_operator_api::OpError::Failed { message } if message.contains("configure")
        ));
        assert!(
            out.is_empty(),
            "a failed on_event must not have published anything"
        );
    }
}
