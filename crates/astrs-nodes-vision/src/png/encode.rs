//! PNG encoding: [`ImageBuffer`] -> filtered scanlines -> deflated `IDAT` ->
//! `IHDR`/`IDAT`/`IEND` chunk stream.

use crate::buffer::ImageBuffer;
use crate::pixel::PixelFormat;

use super::PngError;
use super::chunk::{SIGNATURE, write_chunk};
use super::filter::{FilterType, choose_filter, filter_scanline};

/// How [`super::encode`] picks each scanline's filter type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilterStrategy {
    /// Every scanline filtered with type `0` (`None`, i.e. unfiltered).
    /// Fastest to encode, usually the largest output — mainly useful for a
    /// test or a caller that wants a predictable, content-independent byte
    /// stream.
    NoFilter,
    /// Every scanline filtered with whichever of the five types minimises
    /// the PNG spec's own non-normative "minimum sum of absolute
    /// differences" heuristic. Slower, usually noticeably smaller; the
    /// default.
    #[default]
    MinimumSum,
}

/// The PNG colour type and bytes-per-pixel `format` encodes as, or
/// [`None`] for a format PNG (or at least this decoder) has no colour type
/// for.
const fn color_type_for(format: PixelFormat) -> Option<(u8, usize)> {
    match format {
        PixelFormat::Mono8 => Some((0, 1)),
        PixelFormat::Rgb8 => Some((2, 3)),
        PixelFormat::Rgba8 => Some((6, 4)),
        PixelFormat::Bgr8 | PixelFormat::Yuyv | PixelFormat::Uyvy => None,
    }
}

/// Encodes `image` as a complete PNG byte stream.
///
/// `image.format()` must be [`PixelFormat::Mono8`], [`PixelFormat::Rgb8`],
/// or [`PixelFormat::Rgba8`] — PNG's own colour model has no packed-YUV or
/// blue-first-order channel type, so a [`PixelFormat::Bgr8`] or
/// [`PixelFormat::Yuyv`]/[`PixelFormat::Uyvy`] source needs
/// [`crate::color::to_rgb8`] (or [`crate::color::to_rgba8`]) first.
///
/// `image.width()`/`image.height()` must both be nonzero — the PNG `IHDR`
/// chunk has no representation for an empty image (spec §11.2.2), and
/// [`super::decode::decode`] refuses one on the way back in (see
/// [`PngError::InvalidDimensions`]'s other use site); rejecting it here too
/// keeps encode/decode symmetric instead of letting encode silently produce
/// a stream this crate's own decoder always refuses.
///
/// # Errors
///
/// [`PngError::InvalidDimensions`] when `image` is zero-sized (see above);
/// [`PngError::UnsupportedEncodeFormat`] for a format listed above;
/// [`PngError::Zlib`] if the `DEFLATE` compressor fails.
pub(crate) fn encode(image: &ImageBuffer, strategy: FilterStrategy) -> Result<Vec<u8>, PngError> {
    if image.width() == 0 || image.height() == 0 {
        return Err(PngError::InvalidDimensions {
            width: image.width(),
            height: image.height(),
            reason: "width and height must both be nonzero",
        });
    }
    let Some((color_type, bpp)) = color_type_for(image.format()) else {
        return Err(PngError::UnsupportedEncodeFormat {
            format: image.format(),
        });
    };

    let row_bytes = image.row_bytes();
    let mut raw = Vec::with_capacity((row_bytes + 1) * (image.height() as usize));
    let mut previous: Vec<u8> = Vec::new();
    for row in image.rows() {
        let filter = match strategy {
            FilterStrategy::NoFilter => FilterType::None,
            FilterStrategy::MinimumSum => choose_filter(row, &previous, bpp),
        };
        let mut filtered = vec![0u8; row_bytes];
        filter_scanline(filter, row, &previous, bpp, &mut filtered);
        raw.push(filter.as_byte());
        raw.extend_from_slice(&filtered);
        previous = row.to_vec();
    }

    let compressed = oxiarc_deflate::zlib::zlib_compress(&raw, 6)
        .map_err(|source| PngError::Zlib(source.to_string()))?;

    let mut out = Vec::with_capacity(SIGNATURE.len() + 64 + compressed.len());
    out.extend_from_slice(&SIGNATURE);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&image.width().to_be_bytes());
    ihdr.extend_from_slice(&image.height().to_be_bytes());
    ihdr.push(8); // bit depth: this crate is 8-bit-only end to end
    ihdr.push(color_type);
    ihdr.push(0); // compression method: 0 (deflate), the only one the spec defines
    ihdr.push(0); // filter method: 0 (the five per-scanline predictors)
    ihdr.push(0); // interlace method: 0 (no Adam7)
    write_chunk(&mut out, b"IHDR", &ihdr);
    write_chunk(&mut out, b"IDAT", &compressed);
    write_chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::png::decode::decode;

    #[test]
    fn color_type_for_covers_exactly_the_three_encodable_formats() {
        assert_eq!(color_type_for(PixelFormat::Mono8), Some((0, 1)));
        assert_eq!(color_type_for(PixelFormat::Rgb8), Some((2, 3)));
        assert_eq!(color_type_for(PixelFormat::Rgba8), Some((6, 4)));
        assert_eq!(color_type_for(PixelFormat::Bgr8), None);
        assert_eq!(color_type_for(PixelFormat::Yuyv), None);
        assert_eq!(color_type_for(PixelFormat::Uyvy), None);
    }

    #[test]
    fn encode_refuses_a_zero_dimension() {
        // `resize::resize_nearest`/`resize_bilinear` document that resizing
        // *to* 0x0 always succeeds, so a zero-sized `ImageBuffer` is a
        // reachable input here, not just a hypothetical one -- and without
        // this guard `encode` would happily produce a stream whose IHDR
        // declares a `0` dimension, which `decode` (correctly) always
        // refuses (see `parse_ihdr_rejects_a_zero_dimension` in
        // `super::super::decode`'s own tests). Checking symmetrically here
        // turns that into an immediate, specific error instead of a
        // silently undecodable byte stream.
        let empty = ImageBuffer::zeroed(PixelFormat::Mono8, 0, 0).unwrap();
        assert!(matches!(
            encode(&empty, FilterStrategy::NoFilter),
            Err(PngError::InvalidDimensions {
                width: 0,
                height: 0,
                ..
            })
        ));
    }

    #[test]
    fn encode_refuses_bgr8() {
        let image = ImageBuffer::new(PixelFormat::Bgr8, 1, 1, vec![1, 2, 3]).unwrap();
        assert!(matches!(
            encode(&image, FilterStrategy::NoFilter),
            Err(PngError::UnsupportedEncodeFormat { .. })
        ));
    }

    #[test]
    fn a_no_filter_stream_starts_with_the_png_signature() {
        let image = ImageBuffer::new(PixelFormat::Mono8, 2, 2, vec![1, 2, 3, 4]).unwrap();
        let bytes = encode(&image, FilterStrategy::NoFilter).unwrap();
        assert_eq!(&bytes[..8], &SIGNATURE);
        assert_eq!(&bytes[12..16], b"IHDR");
    }

    #[test]
    fn every_format_and_strategy_round_trips() {
        let cases: [(PixelFormat, Vec<u8>, u32, u32); 3] = [
            (
                PixelFormat::Mono8,
                vec![0, 64, 128, 192, 255, 30, 60, 90, 120],
                3,
                3,
            ),
            (PixelFormat::Rgb8, (0..48u8).collect(), 4, 4),
            (PixelFormat::Rgba8, (0..64u8).collect(), 4, 4),
        ];
        for (format, data, width, height) in cases {
            let image = ImageBuffer::new(format, width, height, data).unwrap();
            for strategy in [FilterStrategy::NoFilter, FilterStrategy::MinimumSum] {
                let bytes = encode(&image, strategy).unwrap();
                let decoded = decode(&bytes).unwrap();
                assert_eq!(decoded, image, "{format} {strategy:?}");
            }
        }
    }
}
