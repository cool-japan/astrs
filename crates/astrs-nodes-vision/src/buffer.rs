//! [`ImageBuffer`] — an owned, row-major pixel buffer.
//!
//! [`astrs_data::tensor::ImageView`] is a *read-only* checked view over
//! somebody else's Arrow columns (see that module's own doc) — exactly right
//! for decoding a payload, useless for building one, since there is no way
//! to construct a fresh one from scratch or write through it. Every op in
//! this crate (resize, blur, threshold, draw, ...) needs to *produce* a new
//! frame, so this module is the owned, mutable counterpart: a flat
//! `Vec<u8>` plus the [`PixelFormat`]/width/height that give it meaning,
//! with row-wise accessors instead of `ImageView`'s N-D indexing (a vision
//! op walks scanlines, not arbitrary tensor coordinates).
//!
//! [`ImageBuffer::from_message`]/[`ImageBuffer::to_message`] and
//! [`ImageBuffer::from_view`] are the boundary to the columnar wire: they
//! cover [`PixelFormat::Mono8`]/[`PixelFormat::Rgb8`]/[`PixelFormat::Bgr8`]/
//! [`PixelFormat::Rgba8`] — the four formats `std/media/v1/Image` can also
//! represent. [`PixelFormat::Yuyv`]/[`PixelFormat::Uyvy`] are not in that
//! list (`astrs_data`'s own `pixel_format_channels` has no `yuyv` entry at
//! all): a driver operator decodes them into an `ImageBuffer` directly from
//! the camera's raw bytes, converts to RGB/BGR with [`crate::color`], and
//! only *that* result ever reaches the wire.

use astrs_data::tensor::ImageView;
use astrs_node_api::message::{Image as WireImage, ImageSamples, PixelFormat as WirePixelFormat};

use crate::error::{Result, VisionError};
use crate::pixel::PixelFormat;

/// An owned, tightly-packed, row-major image: [`PixelFormat`] plus
/// width/height plus exactly `format.frame_bytes(width, height)` bytes.
///
/// Every constructor checks that byte count; every op that changes the
/// geometry (resize, ...) returns a fresh, freshly-checked `ImageBuffer`
/// rather than mutating one in place, so a value of this type is always
/// internally consistent — a row index derived from `width`/`height` never
/// runs off the end of `data`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBuffer {
    format: PixelFormat,
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl ImageBuffer {
    /// Wraps `data` as a `width`×`height` frame in `format`.
    ///
    /// # Errors
    ///
    /// [`VisionError::InvalidGeometry`] when `width` is not a multiple of
    /// `format`'s pixel-group size, or the byte count overflows `usize`;
    /// [`VisionError::BufferSizeMismatch`] when `data.len()` is not exactly
    /// the byte count that geometry implies.
    pub fn new(format: PixelFormat, width: u32, height: u32, data: Vec<u8>) -> Result<Self> {
        let expected = frame_bytes_checked(format, width, height)?;
        if data.len() != expected {
            return Err(VisionError::BufferSizeMismatch {
                format,
                width,
                height,
                expected,
                actual: data.len(),
            });
        }
        Ok(Self {
            format,
            width,
            height,
            data,
        })
    }

    /// A `width`×`height` frame in `format`, every byte zero.
    ///
    /// For every format this crate supports, an all-zero frame is a solid
    /// black image (mid-grey for [`PixelFormat::Yuyv`]/[`PixelFormat::Uyvy`]
    /// is `Y=0`, not the conventional `Y=128` — callers wanting a neutral
    /// starting canvas for those two should fill it explicitly).
    ///
    /// # Errors
    ///
    /// As [`ImageBuffer::new`].
    pub fn zeroed(format: PixelFormat, width: u32, height: u32) -> Result<Self> {
        let expected = frame_bytes_checked(format, width, height)?;
        Self::new(format, width, height, vec![0u8; expected])
    }

    /// This frame's pixel format.
    #[must_use]
    pub const fn format(&self) -> PixelFormat {
        self.format
    }

    /// Width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// How many colour channels one pixel holds — see
    /// [`PixelFormat::channels`].
    #[must_use]
    pub const fn channels(&self) -> usize {
        self.format.channels()
    }

    /// How many bytes one pixel occupies on average — see
    /// [`PixelFormat::bytes_per_pixel`].
    #[must_use]
    pub const fn bytes_per_pixel(&self) -> usize {
        self.format.bytes_per_pixel()
    }

    /// The byte length of one tightly-packed row.
    #[must_use]
    pub fn row_bytes(&self) -> usize {
        self.format.row_bytes(self.width as usize).unwrap_or(0)
    }

    /// The backing bytes, one tightly-packed row after another.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// The backing bytes, mutably.
    #[must_use]
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// Consumes the buffer, returning its backing bytes.
    #[must_use]
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }

    /// Row `y`'s bytes, or [`None`] when `y >= height`.
    #[must_use]
    pub fn row(&self, y: u32) -> Option<&[u8]> {
        let row_bytes = self.row_bytes();
        if y >= self.height {
            return None;
        }
        let start = row_bytes.checked_mul(y as usize)?;
        self.data.get(start..start + row_bytes)
    }

    /// Row `y`'s bytes, mutably, or [`None`] when `y >= height`.
    pub fn row_mut(&mut self, y: u32) -> Option<&mut [u8]> {
        let row_bytes = self.row_bytes();
        if y >= self.height {
            return None;
        }
        let start = row_bytes.checked_mul(y as usize)?;
        self.data.get_mut(start..start + row_bytes)
    }

    /// Every row's bytes, top to bottom.
    pub fn rows(&self) -> impl Iterator<Item = &[u8]> {
        let row_bytes = self.row_bytes().max(1);
        self.data.chunks_exact(row_bytes)
    }

    /// Every row's bytes, top to bottom, mutably.
    pub fn rows_mut(&mut self) -> impl Iterator<Item = &mut [u8]> {
        let row_bytes = self.row_bytes().max(1);
        self.data.chunks_exact_mut(row_bytes)
    }

    /// The bytes of the pixel at `(x, y)`, for a
    /// [`PixelFormat::is_fully_sampled`] format.
    ///
    /// [`None`] for an out-of-range coordinate or a sub-sampled format
    /// (whose "pixels" share bytes across a group — index the row directly
    /// instead).
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<&[u8]> {
        if !self.format.is_fully_sampled() || x >= self.width {
            return None;
        }
        let bpp = self.bytes_per_pixel();
        let start = (x as usize).checked_mul(bpp)?;
        self.row(y)?.get(start..start + bpp)
    }

    /// Overwrites the pixel at `(x, y)` with `color`.
    ///
    /// # Errors
    ///
    /// [`VisionError::UnsupportedFormat`] for a sub-sampled format;
    /// [`VisionError::ChannelCountMismatch`] when `color.len()` is not
    /// [`ImageBuffer::bytes_per_pixel`]. An out-of-range `(x, y)` is
    /// silently a no-op — the same "clip, don't fail" convention
    /// [`crate::draw`]'s primitives use for a shape that runs off the
    /// canvas.
    pub fn put_pixel(&mut self, x: u32, y: u32, color: &[u8]) -> Result<()> {
        if !self.format.is_fully_sampled() {
            return Err(VisionError::UnsupportedFormat {
                op: "ImageBuffer::put_pixel",
                format: self.format,
            });
        }
        let bpp = self.bytes_per_pixel();
        if color.len() != bpp {
            return Err(VisionError::ChannelCountMismatch {
                format: self.format,
                expected: bpp,
                actual: color.len(),
            });
        }
        if x >= self.width || y >= self.height {
            return Ok(());
        }
        let start = (x as usize) * bpp;
        if let Some(row) = self.row_mut(y) {
            row[start..start + bpp].copy_from_slice(color);
        }
        Ok(())
    }

    /// [`VisionError::UnsupportedFormat`] naming `op` unless this frame's
    /// format is exactly `expected`.
    ///
    /// The shared guard every single-channel op (threshold, Sobel,
    /// morphology, connected components — see [`crate::error`]'s module
    /// doc) opens with, so each of those modules names its own `op` string
    /// without repeating the comparison.
    pub(crate) fn require_format(&self, expected: PixelFormat, op: &'static str) -> Result<()> {
        if self.format == expected {
            Ok(())
        } else {
            Err(VisionError::UnsupportedFormat {
                op,
                format: self.format,
            })
        }
    }

    /// [`VisionError::UnsupportedFormat`] naming `op` unless this frame's
    /// format [`PixelFormat::is_fully_sampled`].
    ///
    /// The shared guard every geometric/filtering op (resize, blur, draw —
    /// see [`crate::error`]'s module doc) opens with.
    pub(crate) fn require_fully_sampled(&self, op: &'static str) -> Result<()> {
        if self.format.is_fully_sampled() {
            Ok(())
        } else {
            Err(VisionError::UnsupportedFormat {
                op,
                format: self.format,
            })
        }
    }

    /// Builds a frame from a checked [`ImageView`] read off a decoded
    /// `std/media/v1/Image` payload.
    ///
    /// `format` is required rather than inferred: an `ImageView` carries a
    /// bare channel count (from the row's own `channels` field), which
    /// cannot distinguish [`PixelFormat::Rgb8`] from [`PixelFormat::Bgr8`] —
    /// the caller (which read the port's `pixel=` URN parameter) already
    /// knows which one it is.
    ///
    /// # Errors
    ///
    /// [`VisionError::UnsupportedFormat`] when `format` is sub-sampled (no
    /// `ImageView` is ever `yuyv`/`uyvy` — `astrs_data`'s own layout table
    /// has no such `pixel=` value) or the view's samples are not `UInt8`;
    /// [`VisionError::BufferSizeMismatch`] when the view's channel count
    /// does not match `format`'s.
    pub fn from_view(view: &ImageView, format: PixelFormat) -> Result<Self> {
        if !format.is_fully_sampled() {
            return Err(VisionError::UnsupportedFormat {
                op: "ImageBuffer::from_view",
                format,
            });
        }
        let tensor = view.as_u8().ok_or(VisionError::UnsupportedFormat {
            op: "ImageBuffer::from_view (non-8-bit sample column)",
            format,
        })?;
        let width = u32::try_from(view.width()).unwrap_or(u32::MAX);
        let height = u32::try_from(view.height()).unwrap_or(u32::MAX);
        let data = if let Some(slice) = tensor.as_slice() {
            slice.to_vec()
        } else {
            // Defensive fallback: every `ImageView` this crate is ever
            // actually handed is freshly decoded and therefore contiguous
            // (see `ImageView::from_struct_row`), but nothing in that
            // type's contract *promises* contiguity, so a non-contiguous
            // view is read element-by-element rather than assumed away.
            let capacity = view
                .height()
                .saturating_mul(view.width())
                .saturating_mul(view.channels());
            let mut out = Vec::with_capacity(capacity);
            for y in 0..view.height() {
                for x in 0..view.width() {
                    for c in 0..view.channels() {
                        out.push(tensor.get(&[y, x, c])?);
                    }
                }
            }
            out
        };
        Self::new(format, width, height, data)
    }

    /// Builds a frame from a decoded `std/media/v1/Image[pixel=...]`
    /// message.
    ///
    /// # Errors
    ///
    /// [`VisionError::UnsupportedWireFormat`] when the message's samples are
    /// not 8-bit, or its format is not one of
    /// [`PixelFormat::Mono8`]/[`PixelFormat::Rgb8`]/[`PixelFormat::Bgr8`]/
    /// [`PixelFormat::Rgba8`].
    pub fn from_message(image: &WireImage) -> Result<Self> {
        let format = from_wire_format(image.pixel).ok_or(VisionError::UnsupportedWireFormat {
            format: image.pixel,
        })?;
        let ImageSamples::U8(ref data) = image.data else {
            return Err(VisionError::UnsupportedWireFormat {
                format: image.pixel,
            });
        };
        Self::new(format, image.width, image.height, data.clone())
    }

    /// Encodes this frame as a `std/media/v1/Image[pixel=...]` message.
    ///
    /// # Errors
    ///
    /// [`VisionError::UnsupportedFormat`] for [`PixelFormat::Yuyv`]/
    /// [`PixelFormat::Uyvy`] — convert with [`crate::color`] first, since
    /// `std/media/v1/Image` has no packed-4:2:2 representation to send
    /// them as.
    pub fn to_message(&self) -> Result<WireImage> {
        let wire_format = to_wire_format(self.format).ok_or(VisionError::UnsupportedFormat {
            op: "ImageBuffer::to_message",
            format: self.format,
        })?;
        let image = WireImage::new(
            wire_format,
            self.width,
            self.height,
            ImageSamples::U8(self.data.clone()),
        )?;
        Ok(image)
    }
}

/// [`PixelFormat::frame_bytes`], turned into a typed error instead of
/// [`None`].
fn frame_bytes_checked(format: PixelFormat, width: u32, height: u32) -> Result<usize> {
    format
        .frame_bytes(width as usize, height as usize)
        .ok_or_else(|| {
            let reason = if !(width as usize).is_multiple_of(format.pixels_per_group()) {
                "width is not a multiple of the format's pixel-group size"
            } else {
                "the byte count overflows usize"
            };
            VisionError::InvalidGeometry {
                format,
                width,
                height,
                reason,
            }
        })
}

/// The wire [`WirePixelFormat`] equivalent of a fully-sampled
/// [`PixelFormat`], or [`None`] for the two packed 4:2:2 formats.
const fn to_wire_format(format: PixelFormat) -> Option<WirePixelFormat> {
    match format {
        PixelFormat::Mono8 => Some(WirePixelFormat::Mono8),
        PixelFormat::Rgb8 => Some(WirePixelFormat::Rgb8),
        PixelFormat::Bgr8 => Some(WirePixelFormat::Bgr8),
        PixelFormat::Rgba8 => Some(WirePixelFormat::Rgba8),
        PixelFormat::Yuyv | PixelFormat::Uyvy => None,
    }
}

/// The [`PixelFormat`] equivalent of an 8-bit [`WirePixelFormat`], or
/// [`None`] for a 16-bit/float wire format this crate does not hold.
const fn from_wire_format(format: WirePixelFormat) -> Option<PixelFormat> {
    match format {
        WirePixelFormat::Mono8 => Some(PixelFormat::Mono8),
        WirePixelFormat::Rgb8 => Some(PixelFormat::Rgb8),
        WirePixelFormat::Bgr8 => Some(PixelFormat::Bgr8),
        WirePixelFormat::Rgba8 => Some(PixelFormat::Rgba8),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_data::array::{IntoArrayRef, ListArray, StructArray, UInt8Array, UInt32Array};
    use astrs_data::urn::layouts::media::image_layout;
    use astrs_data::{DataType, Field};

    #[test]
    fn new_validates_the_exact_byte_count() {
        assert!(ImageBuffer::new(PixelFormat::Mono8, 2, 2, vec![0; 4]).is_ok());
        let error = ImageBuffer::new(PixelFormat::Mono8, 2, 2, vec![0; 3]).unwrap_err();
        assert!(matches!(
            error,
            VisionError::BufferSizeMismatch {
                expected: 4,
                actual: 3,
                ..
            }
        ));
    }

    #[test]
    fn new_rejects_an_odd_width_yuyv_frame() {
        let error = ImageBuffer::new(PixelFormat::Yuyv, 3, 1, vec![0; 6]).unwrap_err();
        assert!(matches!(error, VisionError::InvalidGeometry { .. }));
    }

    #[test]
    fn zeroed_is_all_zero_and_the_right_size() {
        let image = ImageBuffer::zeroed(PixelFormat::Rgb8, 4, 3).unwrap();
        assert_eq!(image.data().len(), 4 * 3 * 3);
        assert!(image.data().iter().all(|&b| b == 0));
    }

    #[test]
    fn row_access_matches_manual_slicing() {
        let data = (0..16u8).collect::<Vec<_>>(); // 2x2 rgba8: 2 rows of 8 bytes
        let image = ImageBuffer::new(PixelFormat::Rgba8, 2, 2, data.clone()).unwrap();
        assert_eq!(image.row(0), Some(&data[..8]));
        assert_eq!(image.row(1), Some(&data[8..]));
        assert_eq!(image.row(2), None);
        assert_eq!(image.rows().count(), 2);
    }

    #[test]
    fn pixel_and_put_pixel_round_trip() {
        let mut image = ImageBuffer::zeroed(PixelFormat::Rgb8, 3, 2).unwrap();
        image.put_pixel(1, 1, &[10, 20, 30]).unwrap();
        assert_eq!(image.pixel(1, 1), Some(&[10u8, 20, 30][..]));
        assert_eq!(image.pixel(0, 0), Some(&[0u8, 0, 0][..]));
        // Out of range is a silent no-op, not an error.
        image.put_pixel(99, 99, &[1, 2, 3]).unwrap();
    }

    #[test]
    fn put_pixel_rejects_a_wrong_channel_count() {
        let mut image = ImageBuffer::zeroed(PixelFormat::Rgb8, 2, 2).unwrap();
        let error = image.put_pixel(0, 0, &[1, 2]).unwrap_err();
        assert!(matches!(
            error,
            VisionError::ChannelCountMismatch {
                expected: 3,
                actual: 2,
                ..
            }
        ));
    }

    #[test]
    fn pixel_access_is_refused_on_a_packed_yuyv_frame() {
        let image = ImageBuffer::zeroed(PixelFormat::Yuyv, 2, 1).unwrap();
        assert_eq!(image.pixel(0, 0), None);
    }

    #[test]
    fn wire_round_trip_covers_every_fully_sampled_format() {
        for (format, data) in [
            (PixelFormat::Mono8, vec![1u8, 2, 3, 4]),
            (PixelFormat::Rgb8, (0..12u8).collect()),
            (PixelFormat::Bgr8, (0..12u8).collect()),
            (PixelFormat::Rgba8, (0..16u8).collect()),
        ] {
            let image = ImageBuffer::new(format, 2, 2, data).unwrap();
            let message = image.to_message().unwrap();
            let round_tripped = ImageBuffer::from_message(&message).unwrap();
            assert_eq!(round_tripped, image, "{format}");
        }
    }

    #[test]
    fn yuyv_has_no_wire_representation() {
        let image = ImageBuffer::zeroed(PixelFormat::Yuyv, 2, 1).unwrap();
        assert!(matches!(
            image.to_message(),
            Err(VisionError::UnsupportedFormat { .. })
        ));
    }

    #[test]
    fn a_16_bit_wire_image_is_refused() {
        use astrs_node_api::message::{
            Image as WireImage, ImageSamples, PixelFormat as WirePixelFormat,
        };
        let message =
            WireImage::new(WirePixelFormat::Mono16, 2, 1, ImageSamples::U16(vec![1, 2])).unwrap();
        assert!(matches!(
            ImageBuffer::from_message(&message),
            Err(VisionError::UnsupportedWireFormat { .. })
        ));
    }

    fn mono8_view_row(width: u32, height: u32, samples: Vec<u8>) -> StructArray {
        let DataType::Struct(fields) = image_layout("mono8").unwrap() else {
            unreachable!()
        };
        let data = ListArray::try_from_lengths(
            Field::required("sample", DataType::UInt8),
            [samples.len()],
            UInt8Array::from_values(samples).into_array_ref(),
        )
        .unwrap()
        .into_array_ref();
        StructArray::try_new(
            fields,
            vec![
                UInt32Array::from_values([width]).into_array_ref(),
                UInt32Array::from_values([height]).into_array_ref(),
                UInt32Array::from_values([width]).into_array_ref(),
                UInt8Array::from_values([1u8]).into_array_ref(),
                data,
            ],
            None,
        )
        .unwrap()
    }

    #[test]
    fn from_view_reads_a_decoded_image_view() {
        let row = mono8_view_row(2, 2, vec![10, 20, 30, 40]);
        let view = ImageView::from_struct_row(&row, 0).unwrap();
        let image = ImageBuffer::from_view(&view, PixelFormat::Mono8).unwrap();
        assert_eq!(image.width(), 2);
        assert_eq!(image.height(), 2);
        assert_eq!(image.data(), &[10, 20, 30, 40]);
    }

    #[test]
    fn from_view_refuses_a_packed_format() {
        let row = mono8_view_row(2, 1, vec![1, 2]);
        let view = ImageView::from_struct_row(&row, 0).unwrap();
        assert!(matches!(
            ImageBuffer::from_view(&view, PixelFormat::Yuyv),
            Err(VisionError::UnsupportedFormat { .. })
        ));
    }

    #[test]
    fn require_format_accepts_a_match_and_names_the_op_on_a_mismatch() {
        let image = ImageBuffer::zeroed(PixelFormat::Mono8, 2, 2).unwrap();
        assert!(image.require_format(PixelFormat::Mono8, "test::op").is_ok());
        let error = image
            .require_format(PixelFormat::Rgb8, "test::op")
            .unwrap_err();
        assert!(matches!(
            error,
            VisionError::UnsupportedFormat {
                op: "test::op",
                format: PixelFormat::Mono8
            }
        ));
    }

    #[test]
    fn require_fully_sampled_rejects_only_the_packed_formats() {
        for format in PixelFormat::ALL {
            let image = ImageBuffer::zeroed(*format, 2, 2).unwrap();
            let result = image.require_fully_sampled("test::op");
            assert_eq!(result.is_ok(), format.is_fully_sampled(), "{format}");
        }
    }

    #[test]
    fn from_view_reports_a_channel_mismatch_as_a_buffer_size_error() {
        // The view is 1-channel (mono8) but the caller claims 3 (rgb8).
        let row = mono8_view_row(2, 1, vec![1, 2]);
        let view = ImageView::from_struct_row(&row, 0).unwrap();
        assert!(matches!(
            ImageBuffer::from_view(&view, PixelFormat::Rgb8),
            Err(VisionError::BufferSizeMismatch { .. })
        ));
    }
}
