//! PNG decoding: chunk stream -> validated `IHDR` -> concatenated `IDAT` ->
//! inflated scanlines -> unfiltered pixels -> [`ImageBuffer`].

use crate::buffer::ImageBuffer;
use crate::pixel::PixelFormat;

use super::PngError;
use super::chunk::ChunkReader;
use super::filter::{FilterType, unfilter_scanline};

/// The validated contents of an `IHDR` chunk this crate can actually decode:
/// 8-bit-per-sample, no interlacing, colour type grey/RGB/RGBA.
struct Ihdr {
    width: u32,
    height: u32,
    format: PixelFormat,
    /// Bytes per complete pixel — `format.bytes_per_pixel()`, kept alongside
    /// it so nothing downstream needs to re-derive it from `format` (and
    /// risk an unreachable arm for the two YUV422 variants `format` can
    /// never actually be here).
    bpp: usize,
}

/// Parses and validates a 13-byte `IHDR` chunk body (spec §11.2.2).
fn parse_ihdr(data: &[u8]) -> Result<Ihdr, PngError> {
    let [
        w0,
        w1,
        w2,
        w3,
        h0,
        h1,
        h2,
        h3,
        bit_depth,
        color_type,
        compression,
        filter_method,
        interlace,
    ] = *data
        .first_chunk::<13>()
        .ok_or(PngError::InvalidIhdrLength { found: data.len() })?;
    let width = u32::from_be_bytes([w0, w1, w2, w3]);
    let height = u32::from_be_bytes([h0, h1, h2, h3]);
    if width == 0 || height == 0 {
        return Err(PngError::InvalidDimensions {
            width,
            height,
            reason: "width and height must both be nonzero",
        });
    }
    if compression != 0 {
        return Err(PngError::UnsupportedCompressionMethod { found: compression });
    }
    if filter_method != 0 {
        return Err(PngError::UnsupportedFilterMethod {
            found: filter_method,
        });
    }
    if interlace != 0 {
        return Err(PngError::UnsupportedInterlace { found: interlace });
    }
    if bit_depth != 8 {
        return Err(PngError::UnsupportedBitDepth { found: bit_depth });
    }
    let (format, bpp) = match color_type {
        0 => (PixelFormat::Mono8, 1usize),
        2 => (PixelFormat::Rgb8, 3usize),
        6 => (PixelFormat::Rgba8, 4usize),
        3 => {
            return Err(PngError::UnsupportedColorType {
                found: 3,
                reason: "palette (indexed-colour) PNGs are out of scope",
            });
        }
        4 => {
            return Err(PngError::UnsupportedColorType {
                found: 4,
                reason: "grey+alpha has no astrs-nodes-vision pixel format",
            });
        }
        other => {
            return Err(PngError::UnsupportedColorType {
                found: other,
                reason: "not one of the PNG spec's five defined colour types",
            });
        }
    };
    Ok(Ihdr {
        width,
        height,
        format,
        bpp,
    })
}

/// Whether a chunk type is "critical" (spec §5.4: uppercase first letter) —
/// one a conformant decoder must understand or refuse, as opposed to an
/// ancillary chunk (`tEXt`, `pHYs`, `gAMA`, ...) it is always safe to skip.
fn is_critical(kind: [u8; 4]) -> bool {
    kind[0].is_ascii_uppercase()
}

/// Decodes a complete PNG byte stream.
///
/// # Errors
///
/// See [`PngError`]'s variants — a malformed signature or chunk stream, an
/// `IHDR` this crate's 8-bit grey/RGB/RGBA-only, non-interlaced decoder
/// cannot handle, a missing `IHDR`/`IDAT`/`IEND`, an unrecognised critical
/// chunk, a `zlib`/`DEFLATE` stream error, or decompressed data that is not
/// exactly the byte count the header implies.
pub(crate) fn decode(bytes: &[u8]) -> Result<ImageBuffer, PngError> {
    let reader = ChunkReader::new(bytes)?;
    let mut ihdr: Option<Ihdr> = None;
    let mut idat = Vec::new();
    let mut seen_iend = false;

    for chunk in reader {
        let chunk = chunk?;
        if chunk.kind == *b"IHDR" {
            if ihdr.is_some() {
                return Err(PngError::DuplicateIhdr);
            }
            ihdr = Some(parse_ihdr(chunk.data)?);
        } else if chunk.kind == *b"IDAT" {
            if ihdr.is_none() {
                return Err(PngError::IdatBeforeIhdr);
            }
            idat.extend_from_slice(chunk.data);
        } else if chunk.kind == *b"IEND" {
            seen_iend = true;
        } else if chunk.kind == *b"PLTE" {
            // Recognised and skipped: none of the colour types this crate
            // decodes (grey, RGB, RGBA) is ever paired with a palette, so a
            // PLTE alongside them carries no information a decoder is
            // required to act on (spec §11.2.3).
        } else if is_critical(chunk.kind) {
            return Err(PngError::UnsupportedCriticalChunk {
                kind: chunk.kind_str(),
            });
        }
        // An ancillary chunk (tEXt, pHYs, gAMA, ...): its CRC-32 was already
        // verified by the reader; safe to ignore.
    }

    let ihdr = ihdr.ok_or(PngError::MissingIhdr)?;
    if idat.is_empty() {
        return Err(PngError::MissingIdat);
    }
    if !seen_iend {
        return Err(PngError::MissingIend);
    }

    let decompressed = oxiarc_deflate::zlib::zlib_decompress(&idat)
        .map_err(|source| PngError::Zlib(source.to_string()))?;

    let width = ihdr.width as usize;
    let height = ihdr.height as usize;
    let row_bytes = width.saturating_mul(ihdr.bpp);
    let stride = row_bytes.saturating_add(1); // +1: the filter-type byte
    let expected_len = stride.saturating_mul(height);
    if decompressed.len() != expected_len {
        return Err(PngError::ImageDataSizeMismatch {
            expected: expected_len,
            actual: decompressed.len(),
        });
    }

    let mut pixels = vec![0u8; row_bytes * height];
    let mut previous: Vec<u8> = Vec::new();
    for y in 0..height {
        let scanline_start = y * stride;
        let filter_byte = decompressed[scanline_start];
        let filter = FilterType::from_byte(filter_byte)
            .ok_or(PngError::InvalidFilterType { found: filter_byte })?;
        let mut current = decompressed[scanline_start + 1..scanline_start + 1 + row_bytes].to_vec();
        unfilter_scanline(filter, &mut current, &previous, ihdr.bpp);
        let out_start = y * row_bytes;
        pixels[out_start..out_start + row_bytes].copy_from_slice(&current);
        previous = current;
    }

    ImageBuffer::new(ihdr.format, ihdr.width, ihdr.height, pixels)
        .map_err(|source| PngError::Image(source.to_string()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::png::chunk::write_chunk;
    use crate::png::encode::{FilterStrategy, encode};

    fn png_bytes(image: &ImageBuffer, strategy: FilterStrategy) -> Vec<u8> {
        encode(image, strategy).unwrap()
    }

    #[test]
    fn parse_ihdr_rejects_a_wrong_length() {
        assert!(matches!(
            parse_ihdr(&[0; 12]),
            Err(PngError::InvalidIhdrLength { found: 12 })
        ));
    }

    #[test]
    fn parse_ihdr_reads_a_well_formed_header() {
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&3u32.to_be_bytes());
        data.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB, no interlace
        let ihdr = parse_ihdr(&data).unwrap();
        assert_eq!((ihdr.width, ihdr.height), (4, 3));
        assert_eq!(ihdr.format, PixelFormat::Rgb8);
        assert_eq!(ihdr.bpp, 3);
    }

    #[test]
    fn parse_ihdr_rejects_every_unsupported_field_distinctly() {
        let base = |patch: [(usize, u8); 1]| {
            let mut data = vec![0, 0, 0, 2, 0, 0, 0, 2, 8, 2, 0, 0, 0];
            for (index, value) in patch {
                data[index] = value;
            }
            data
        };
        assert!(matches!(
            parse_ihdr(&base([(8, 4)])), // bit depth 4
            Err(PngError::UnsupportedBitDepth { found: 4 })
        ));
        assert!(matches!(
            parse_ihdr(&base([(9, 3)])), // palette
            Err(PngError::UnsupportedColorType { found: 3, .. })
        ));
        assert!(matches!(
            parse_ihdr(&base([(9, 4)])), // grey+alpha
            Err(PngError::UnsupportedColorType { found: 4, .. })
        ));
        assert!(matches!(
            parse_ihdr(&base([(9, 7)])), // not a defined colour type
            Err(PngError::UnsupportedColorType { found: 7, .. })
        ));
        assert!(matches!(
            parse_ihdr(&base([(10, 1)])), // compression method
            Err(PngError::UnsupportedCompressionMethod { found: 1 })
        ));
        assert!(matches!(
            parse_ihdr(&base([(11, 1)])), // filter method
            Err(PngError::UnsupportedFilterMethod { found: 1 })
        ));
        assert!(matches!(
            parse_ihdr(&base([(12, 1)])), // Adam7 interlace
            Err(PngError::UnsupportedInterlace { found: 1 })
        ));
    }

    #[test]
    fn parse_ihdr_rejects_a_zero_dimension() {
        let mut data = vec![0, 0, 0, 0, 0, 0, 0, 2, 8, 0, 0, 0, 0];
        assert!(matches!(
            parse_ihdr(&data),
            Err(PngError::InvalidDimensions { width: 0, .. })
        ));
        data[3] = 2;
        data[7] = 0;
        assert!(matches!(
            parse_ihdr(&data),
            Err(PngError::InvalidDimensions { height: 0, .. })
        ));
    }

    #[test]
    fn a_stream_missing_ihdr_is_reported() {
        // No IDAT either: an IDAT with no preceding IHDR hits the more
        // specific `IdatBeforeIhdr` (see `an_idat_before_ihdr_is_reported`
        // below) before the loop ever finishes to notice IHDR is missing
        // altogether.
        let mut stream = crate::png::chunk::SIGNATURE.to_vec();
        write_chunk(&mut stream, b"IEND", b"");
        assert!(matches!(decode(&stream), Err(PngError::MissingIhdr)));
    }

    #[test]
    fn a_stream_missing_idat_is_reported() {
        let mut stream = crate::png::chunk::SIGNATURE.to_vec();
        let mut ihdr_data = Vec::new();
        ihdr_data.extend_from_slice(&1u32.to_be_bytes());
        ihdr_data.extend_from_slice(&1u32.to_be_bytes());
        ihdr_data.extend_from_slice(&[8, 0, 0, 0, 0]);
        write_chunk(&mut stream, b"IHDR", &ihdr_data);
        write_chunk(&mut stream, b"IEND", b"");
        assert!(matches!(decode(&stream), Err(PngError::MissingIdat)));
    }

    #[test]
    fn an_idat_before_ihdr_is_reported() {
        let mut stream = crate::png::chunk::SIGNATURE.to_vec();
        write_chunk(&mut stream, b"IDAT", b"x");
        assert!(matches!(decode(&stream), Err(PngError::IdatBeforeIhdr)));
    }

    #[test]
    fn an_unrecognised_critical_chunk_is_reported() {
        let image = ImageBuffer::new(PixelFormat::Mono8, 1, 1, vec![9]).unwrap();
        let mut stream = png_bytes(&image, FilterStrategy::NoFilter);
        // Insert a fabricated, uppercase-led (= critical, spec §5.4) chunk
        // type this decoder has never heard of, right before IEND.
        let insert_at = stream.len() - 12; // 12 = length+type+crc of an empty IEND
        let mut critical = Vec::new();
        write_chunk(&mut critical, b"XxXx", b"");
        stream.splice(insert_at..insert_at, critical);
        assert!(matches!(
            decode(&stream),
            Err(PngError::UnsupportedCriticalChunk { .. })
        ));
    }

    #[test]
    fn an_unrecognised_ancillary_chunk_is_silently_skipped() {
        let image = ImageBuffer::new(PixelFormat::Mono8, 1, 1, vec![9]).unwrap();
        let mut stream = png_bytes(&image, FilterStrategy::NoFilter);
        let insert_at = stream.len() - 12;
        let mut ancillary = Vec::new();
        write_chunk(&mut ancillary, b"tEXt", b"hello=world");
        stream.splice(insert_at..insert_at, ancillary);
        assert_eq!(decode(&stream).unwrap(), image);
    }

    #[test]
    fn a_scanline_with_a_bad_filter_byte_is_reported() {
        // 1x1 mono8: IHDR says 8-bit grey, but the (unfiltered by us)
        // scanline's leading filter-type byte is 9, which names no filter.
        let mut ihdr_data = Vec::new();
        ihdr_data.extend_from_slice(&1u32.to_be_bytes());
        ihdr_data.extend_from_slice(&1u32.to_be_bytes());
        ihdr_data.extend_from_slice(&[8, 0, 0, 0, 0]);
        let raw = [9u8, 200]; // bad filter byte, one grey sample
        let compressed = oxiarc_deflate::zlib::zlib_compress(&raw, 6).unwrap();
        let mut stream = crate::png::chunk::SIGNATURE.to_vec();
        write_chunk(&mut stream, b"IHDR", &ihdr_data);
        write_chunk(&mut stream, b"IDAT", &compressed);
        write_chunk(&mut stream, b"IEND", b"");
        assert!(matches!(
            decode(&stream),
            Err(PngError::InvalidFilterType { found: 9 })
        ));
    }

    #[test]
    fn plte_is_recognised_and_skipped_for_a_non_palette_image() {
        let image = ImageBuffer::new(PixelFormat::Rgb8, 1, 1, vec![10, 20, 30]).unwrap();
        let mut stream = png_bytes(&image, FilterStrategy::NoFilter);
        let insert_at = stream.len() - 12;
        let mut plte = Vec::new();
        write_chunk(&mut plte, b"PLTE", &[0, 0, 0, 255, 255, 255]);
        stream.splice(insert_at..insert_at, plte);
        assert_eq!(decode(&stream).unwrap(), image);
    }
}
