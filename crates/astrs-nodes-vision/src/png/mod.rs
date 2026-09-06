//! A hand-rolled, pure-Rust PNG codec (spec: ISO/IEC 15948, W3C PNG 1.2).
//!
//! Image handling is where C creeps into a robotics stack — `libpng`,
//! `libjpeg-turbo`, OpenCV's own bundled codecs. This module links none of
//! them: chunk framing, the CRC-32, the five scanline filters, and `IHDR`
//! parsing are all written directly against the spec in this crate, and the
//! one piece that would otherwise pull in `flate2`/`miniz_oxide` (both
//! banned by `deny.toml`) — `IDAT`'s `zlib`-wrapped `DEFLATE` stream — runs
//! on `oxiarc-deflate`, the COOLJAPAN pure-Rust replacement.
//!
//! # Scope
//!
//! 8 bits per sample, colour type grey (`0`)/RGB (`2`)/RGBA (`6`) — i.e.
//! exactly [`PixelFormat::Mono8`]/[`PixelFormat::Rgb8`]/[`PixelFormat::Rgba8`]
//! — no interlacing, every scanline filter (spec §6: `None`/`Sub`/`Up`/
//! `Average`/`Paeth`). Decoding recognises `IHDR`/`PLTE`/`IDAT`/`IEND` by
//! name and every other chunk by its criticality bit (spec §5.4): an
//! ancillary chunk (`tEXt`, `pHYs`, `gAMA`, ...) is skipped once its CRC-32
//! checks out; an unrecognised *critical* chunk is refused rather than
//! silently ignored, since a decoder is not allowed to guess what one
//! means. Indexed colour (palette, colour type `3`), grey+alpha (colour
//! type `4`), 16-bit samples, and Adam7 interlacing are explicitly out of
//! scope — none has an [`PixelFormat`] to decode into. **JPEG is out of
//! scope entirely**: use [`astrs_node_api::message::CompressedImage`]'s
//! `format` field to carry one through the graph unopened if a driver
//! produces it, but this crate has no decoder for it.
//!
//! # Examples
//!
//! ```
//! use astrs_nodes_vision::png::{FilterStrategy, decode, encode};
//! use astrs_nodes_vision::{ImageBuffer, PixelFormat};
//!
//! // A tiny 2x2 RGB checkerboard, built in memory (no filesystem, no
//! // external fixture — the same shape this crate's own test suite uses
//! // for "a couple of tiny real PNGs generated in-test").
//! let checkerboard = ImageBuffer::new(
//!     PixelFormat::Rgb8,
//!     2,
//!     2,
//!     vec![
//!         255, 255, 255, /**/ 0, 0, 0, //
//!         0, 0, 0, /**/ 255, 255, 255,
//!     ],
//! )?;
//!
//! let bytes = encode(&checkerboard, FilterStrategy::MinimumSum)?;
//! assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n"); // the PNG signature
//! assert_eq!(decode(&bytes)?, checkerboard);
//! # Ok::<(), astrs_nodes_vision::VisionError>(())
//! ```

mod chunk;
mod crc32;
mod decode;
mod encode;
mod filter;

pub use encode::FilterStrategy;

use crate::buffer::ImageBuffer;
use crate::error::Result;
use crate::pixel::PixelFormat;

/// A PNG stream failed to parse, validate, or (de)compress, or an
/// [`ImageBuffer`] could not be PNG-encoded in the first place.
///
/// `#[non_exhaustive]`: new PNG spec features this crate learns to reject
/// with a more specific variant (rather than falling through to a generic
/// one) may add entries here without that being a breaking change.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PngError {
    /// The stream is shorter than the 8-byte PNG signature, or does not
    /// start with it.
    #[error("not a PNG stream: bad or missing signature")]
    BadSignature,

    /// The chunk stream ended in the middle of a chunk's length, type,
    /// data, or CRC-32.
    #[error("PNG chunk stream ends unexpectedly")]
    TruncatedChunk,

    /// A chunk declared a length longer than the spec's own `2^31 - 1`
    /// byte limit (spec §5.3).
    #[error("PNG chunk declares an implausible length ({length} bytes)")]
    ChunkTooLarge {
        /// The rejected length, as the chunk header declared it.
        length: u32,
    },

    /// A chunk's data did not match its trailing CRC-32.
    #[error(
        "PNG chunk {kind:?} failed its CRC-32 check (header says {expected:#010x}, computed {actual:#010x})"
    )]
    CrcMismatch {
        /// The chunk's four-letter type name.
        kind: String,
        /// The CRC-32 the chunk header declared.
        expected: u32,
        /// The CRC-32 actually computed over the chunk's type and data.
        actual: u32,
    },

    /// The stream has no `IHDR` chunk.
    #[error("PNG stream has no IHDR chunk")]
    MissingIhdr,

    /// The stream has more than one `IHDR` chunk (spec §5.6: exactly one,
    /// first).
    #[error("PNG stream has more than one IHDR chunk")]
    DuplicateIhdr,

    /// The `IHDR` chunk's data is not exactly the 13 bytes spec §11.2.2
    /// defines.
    #[error("PNG IHDR chunk is {found} bytes, must be exactly 13")]
    InvalidIhdrLength {
        /// The length actually found.
        found: usize,
    },

    /// The image's width or height is zero.
    #[error("PNG dimensions {width}x{height} are invalid: {reason}")]
    InvalidDimensions {
        /// The declared width.
        width: u32,
        /// The declared height.
        height: u32,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// A bit depth other than `8` — this crate is 8-bit-per-sample only.
    #[error(
        "PNG bit depth {found} is not supported; astrs-nodes-vision only decodes 8-bit-per-sample PNGs"
    )]
    UnsupportedBitDepth {
        /// The bit depth `IHDR` declared.
        found: u8,
    },

    /// A colour type this crate has no [`PixelFormat`] for.
    #[error("PNG colour type {found} is not supported: {reason}")]
    UnsupportedColorType {
        /// The colour type `IHDR` declared.
        found: u8,
        /// Why it is out of scope.
        reason: &'static str,
    },

    /// An interlace method other than `0` (no interlacing) — Adam7 is out
    /// of scope.
    #[error("PNG interlace method {found} is not supported; only method 0 (no interlace) decodes")]
    UnsupportedInterlace {
        /// The interlace method `IHDR` declared.
        found: u8,
    },

    /// A compression method other than `0` (the only one the spec itself
    /// defines).
    #[error("PNG compression method {found} is not supported; only method 0 (deflate) is defined")]
    UnsupportedCompressionMethod {
        /// The compression method `IHDR` declared.
        found: u8,
    },

    /// A filter method other than `0` (the only one the spec itself
    /// defines).
    #[error("PNG filter method {found} is not supported; only method 0 is defined")]
    UnsupportedFilterMethod {
        /// The filter method `IHDR` declared.
        found: u8,
    },

    /// An `IDAT` chunk arrived before `IHDR`.
    #[error("PNG stream has an IDAT chunk before its IHDR")]
    IdatBeforeIhdr,

    /// The stream has no `IDAT` chunk.
    #[error("PNG stream has no IDAT chunk")]
    MissingIdat,

    /// The stream has no `IEND` chunk.
    #[error("PNG stream has no IEND chunk")]
    MissingIend,

    /// A chunk type this decoder does not recognise, whose name marks it
    /// critical (spec §5.4: an uppercase first letter) — an ancillary
    /// chunk in the same position is skipped instead, never an error.
    #[error(
        "PNG stream has an unrecognised critical chunk {kind:?}; refusing rather than guessing what it means"
    )]
    UnsupportedCriticalChunk {
        /// The unrecognised chunk's four-letter type name.
        kind: String,
    },

    /// A scanline's leading filter-type byte named none of the five
    /// defined filters (spec §6.2: `0`-`4`).
    #[error("PNG scanline filter byte {found} is not one of the five defined filter types (0-4)")]
    InvalidFilterType {
        /// The out-of-range byte actually found.
        found: u8,
    },

    /// The `IDAT` stream inflated to a different byte count than
    /// `IHDR`'s width/height/colour-type imply.
    #[error("decompressed PNG image data is {actual} bytes, expected exactly {expected}")]
    ImageDataSizeMismatch {
        /// The byte count `IHDR`'s geometry implies.
        expected: usize,
        /// The byte count `zlib` actually produced.
        actual: usize,
    },

    /// The `zlib`/`DEFLATE` compressor or decompressor
    /// ([`oxiarc_deflate::zlib`]) reported a failure.
    #[error("zlib stream error: {0}")]
    Zlib(String),

    /// Decoding produced pixel bytes an [`ImageBuffer`] refused — in
    /// practice unreachable (this module always sizes the buffer to match
    /// exactly what it decoded), kept as a typed error rather than an
    /// internal `unwrap`.
    #[error("PNG decode produced an invalid image buffer: {0}")]
    Image(String),

    /// [`encode`] was asked to encode a [`PixelFormat`] PNG's colour model
    /// has no chunk-of-bytes representation for.
    #[error(
        "cannot PNG-encode pixel format {format}; convert with astrs_nodes_vision::color first"
    )]
    UnsupportedEncodeFormat {
        /// The format that was refused.
        format: PixelFormat,
    },

    /// [`decode_compressed_image`] was handed an
    /// [`astrs_node_api::message::CompressedImage`] whose `format` field is
    /// not `"png"`.
    #[error("CompressedImage format is {found:?}, not \"png\"")]
    NotPng {
        /// The format the message actually declared.
        found: String,
    },
}

/// Decodes a complete PNG byte stream into an [`ImageBuffer`].
///
/// # Errors
///
/// See [`PngError`]'s variants.
pub fn decode(bytes: &[u8]) -> Result<ImageBuffer> {
    Ok(decode::decode(bytes)?)
}

/// Encodes `image` as a complete PNG byte stream.
///
/// # Errors
///
/// [`PngError::InvalidDimensions`] (wrapped as [`crate::VisionError::Png`])
/// when `image` is zero-sized; [`PngError::UnsupportedEncodeFormat`]
/// (likewise wrapped) unless `image.format()` is
/// [`PixelFormat::Mono8`]/[`PixelFormat::Rgb8`]/[`PixelFormat::Rgba8`].
pub fn encode(image: &ImageBuffer, strategy: FilterStrategy) -> Result<Vec<u8>> {
    Ok(encode::encode(image, strategy)?)
}

/// Decodes an [`astrs_node_api::message::CompressedImage`] whose `format`
/// is `"png"`.
///
/// # Errors
///
/// [`PngError::NotPng`] when `image.format != "png"`; otherwise as
/// [`decode`].
pub fn decode_compressed_image(
    image: &astrs_node_api::message::CompressedImage,
) -> Result<ImageBuffer> {
    if image.format != "png" {
        return Err(PngError::NotPng {
            found: image.format.clone(),
        }
        .into());
    }
    decode(&image.data)
}

/// Encodes `image` as PNG and wraps the bytes as an
/// [`astrs_node_api::message::CompressedImage`] with `format: "png"`.
///
/// # Errors
///
/// As [`encode`].
pub fn encode_to_compressed_image(
    image: &ImageBuffer,
    strategy: FilterStrategy,
) -> Result<astrs_node_api::message::CompressedImage> {
    let bytes = encode(image, strategy)?;
    Ok(astrs_node_api::message::CompressedImage::new("png", bytes))
}

/// An [`astrs_operator_api::Operator`] applying [`encode`] to every `image`
/// input, publishing the PNG bytes on `compressed` as a
/// `std/media/v1/CompressedImage`.
///
/// # Configuration
///
/// | Key | Type | Default | Meaning |
/// |---|---|---|---|
/// | `strategy` | string | `"minimum_sum"` | `"no_filter"` or `"minimum_sum"` — see [`FilterStrategy`] |
#[derive(Debug, Default)]
pub struct PngEncodeOperator {
    strategy: FilterStrategy,
}

impl astrs_operator_api::Operator for PngEncodeOperator {
    fn configure(
        &mut self,
        config: &std::collections::BTreeMap<String, astrs_wire::Parameter>,
    ) -> astrs_operator_api::OpResult<()> {
        self.strategy = match config
            .get("strategy")
            .and_then(astrs_wire::Parameter::as_str)
        {
            Some("no_filter") => FilterStrategy::NoFilter,
            Some("minimum_sum") | None => FilterStrategy::MinimumSum,
            Some(other) => {
                return Err(astrs_operator_api::OpError::failed(format!(
                    "png encode strategy must be \"no_filter\" or \"minimum_sum\", got {other:?}"
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
        match event {
            astrs_operator_api::OpEvent::Input {
                metadata, payload, ..
            } => {
                let image = crate::ops::decode_image(payload)?;
                let compressed = encode_to_compressed_image(&image, self.strategy)?;
                out.send("compressed", metadata.clone(), &compressed)?;
                Ok(astrs_operator_api::Status::Continue)
            }
            astrs_operator_api::OpEvent::Stop { .. } => Ok(astrs_operator_api::Status::Finished),
            _ => Ok(astrs_operator_api::Status::Continue),
        }
    }
}

/// An [`astrs_operator_api::Operator`] applying [`decode`] to every
/// `std/media/v1/CompressedImage` `compressed` input (refusing any whose
/// `format` is not `"png"`), publishing the decoded frame on `image`.
#[derive(Debug, Default)]
pub struct PngDecodeOperator;

impl astrs_operator_api::Operator for PngDecodeOperator {
    fn on_event(
        &mut self,
        event: &astrs_operator_api::OpEvent,
        out: &mut astrs_operator_api::OpOutput,
    ) -> astrs_operator_api::OpResult<astrs_operator_api::Status> {
        match event {
            astrs_operator_api::OpEvent::Input {
                metadata, payload, ..
            } => {
                let batch = astrs_data::ipc::decode_payload(payload)?;
                let compressed = <astrs_node_api::message::CompressedImage as astrs_node_api::message::AstrsMessage>::from_record_batch(&batch)?;
                let image = decode_compressed_image(&compressed)?;
                crate::ops::encode_image(out, "image", metadata.clone(), &image)?;
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

    #[test]
    fn a_compressed_image_with_the_wrong_format_is_refused() {
        let jpeg = astrs_node_api::message::CompressedImage::new("jpeg", vec![0xFF, 0xD8]);
        assert!(matches!(
            decode_compressed_image(&jpeg),
            Err(crate::VisionError::Png(PngError::NotPng { .. }))
        ));
    }

    #[test]
    fn compressed_image_round_trip() {
        let image =
            ImageBuffer::new(PixelFormat::Rgba8, 2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        let compressed = encode_to_compressed_image(&image, FilterStrategy::MinimumSum).unwrap();
        assert_eq!(compressed.format, "png");
        let decoded = decode_compressed_image(&compressed).unwrap();
        assert_eq!(decoded, image);
    }

    #[test]
    fn two_tiny_real_pngs_round_trip_through_this_crates_own_encoder() {
        // "Tiny real PNGs" generated in-test, not sourced from disk: a 1x1
        // solid pixel and a 3x2 gradient, each pushed through a genuine
        // encode -> byte stream -> decode round trip.
        let solid = ImageBuffer::new(PixelFormat::Rgb8, 1, 1, vec![200, 100, 50]).unwrap();
        let solid_bytes = encode(&solid, FilterStrategy::MinimumSum).unwrap();
        assert_eq!(decode(&solid_bytes).unwrap(), solid);

        let gradient_data: Vec<u8> = (0..18u8).collect(); // 3x2 rgb8
        let gradient = ImageBuffer::new(PixelFormat::Rgb8, 3, 2, gradient_data).unwrap();
        let gradient_bytes = encode(&gradient, FilterStrategy::MinimumSum).unwrap();
        assert_eq!(decode(&gradient_bytes).unwrap(), gradient);
        // A real PNG stream: signature, then IHDR immediately, then IEND
        // as literally the last 12 bytes.
        assert_eq!(&gradient_bytes[..8], &chunk::SIGNATURE);
        assert_eq!(
            &gradient_bytes[gradient_bytes.len() - 8..gradient_bytes.len() - 4],
            b"IEND"
        );
    }

    // ---- Operators ----
    //
    // `PngEncodeOperator` and `PngDecodeOperator` are each other's mirror
    // image (`Image` payload -> `CompressedImage` payload -> `Image`
    // payload again), so chaining one instance of each through a real
    // `OpEvent::Input`/`OpOutput` pair -- payload to payload, the way a
    // runtime host actually would -- is the direct analogue of
    // `astrs-nodes-signal`'s own `rfft_then_irfft_operators_round_trip_a_frame`
    // test for this crate's encode/decode operator pair.

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
    fn png_encode_then_png_decode_operators_round_trip_a_frame() {
        use astrs_operator_api::{OpOutput, Operator, Status};

        let image = ImageBuffer::new(PixelFormat::Rgb8, 3, 2, (0..18u8).collect()).unwrap();

        let mut encode_op = PngEncodeOperator::default();
        let mut decode_op = PngDecodeOperator;
        let mut out = OpOutput::new();

        let status = encode_op
            .on_event(&input_event(image_payload(&image)), &mut out)
            .unwrap();
        assert_eq!(status, Status::Continue);
        let sends = out.drain();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].id().as_str(), "compressed");
        let compressed_payload = sends[0].payload().to_vec();

        let status = decode_op
            .on_event(&input_event(compressed_payload), &mut out)
            .unwrap();
        assert_eq!(status, Status::Continue);
        let sends = out.drain();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].id().as_str(), "image");
        let round_tripped = decode_image_payload(sends[0].payload());
        assert_eq!(round_tripped, image);
    }

    #[test]
    fn png_operators_finish_on_stop() {
        use astrs_operator_api::{OpEvent, OpOutput, Operator, Status};

        let stop = OpEvent::Stop {
            cause: astrs_wire::StopCause::Requested,
            grace: None,
        };
        let mut out = OpOutput::new();
        assert_eq!(
            PngEncodeOperator::default()
                .on_event(&stop, &mut out)
                .unwrap(),
            Status::Finished
        );
        assert_eq!(
            PngDecodeOperator.on_event(&stop, &mut out).unwrap(),
            Status::Finished
        );
        assert!(out.is_empty());
    }
}
