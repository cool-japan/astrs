//! `std/media/v1` layouts — images and audio.
//!
//! `Image` and `AudioFrame` compute their layout from a required parameter
//! (`pixel`, `sample`) that selects the sample's columnar type; `stride`,
//! `width`, `height`, `rate` and `channels` are accepted but never drive the
//! *shape* — a port's `Image[pixel=rgb8,width=640,height=480]` and a bare
//! `Image[pixel=rgb8]` resolve to the identical [`DataType`], because both
//! still carry `width`/`height` as row data (see [`image_layout`]).
//! `CompressedImage`'s `format` is required but is itself carried as row
//! data (`format: Utf8`), so its layout does not need a resolver at all.

use crate::datatype::{DataType, Field, Schema};
use crate::urn::error::TypeUrnError;
use crate::urn::parse::TypeUrn;
use crate::urn::registry::{STD_MEDIA_AUDIO_FRAME, STD_MEDIA_IMAGE};

/// The columnar sample type and channel count a `std/media/v1/Image`
/// `pixel` value maps onto.
///
/// The channel count is informational here (`image_layout` also stores it as
/// a row field so [`crate::tensor::ImageView`] never has to divide to
/// recover it) — see that module for the accessor this table exists to feed.
///
/// # Errors
///
/// [`TypeUrnError::UnsupportedParameterValue`] for a `pixel` string this
/// build does not map.
///
/// ```
/// use astrs_data::urn::layouts::media::pixel_format_channels;
/// use astrs_data::DataType;
///
/// assert_eq!(pixel_format_channels("rgb8"), Ok((DataType::UInt8, 3)));
/// assert_eq!(pixel_format_channels("mono16"), Ok((DataType::UInt16, 1)));
/// assert!(pixel_format_channels("nope").is_err());
/// ```
pub fn pixel_format_channels(pixel: &str) -> Result<(DataType, u8), TypeUrnError> {
    match pixel {
        "mono8" => Ok((DataType::UInt8, 1)),
        "mono16" => Ok((DataType::UInt16, 1)),
        "mono32f" => Ok((DataType::Float32, 1)),
        "rgb8" | "bgr8" => Ok((DataType::UInt8, 3)),
        "rgba8" | "bgra8" => Ok((DataType::UInt8, 4)),
        "rgb16" | "bgr16" => Ok((DataType::UInt16, 3)),
        "rgba16" | "bgra16" => Ok((DataType::UInt16, 4)),
        "rgb32f" => Ok((DataType::Float32, 3)),
        "rgba32f" => Ok((DataType::Float32, 4)),
        "bayer_rggb8" | "bayer_bggr8" | "bayer_gbrg8" | "bayer_grbg8" => Ok((DataType::UInt8, 1)),
        other => Err(TypeUrnError::UnsupportedParameterValue {
            urn: STD_MEDIA_IMAGE.to_owned(),
            key: "pixel".to_owned(),
            value: other.to_owned(),
        }),
    }
}

/// `std/media/v1/Image[pixel=…]` — a raw image frame.
///
/// `{width: UInt32, height: UInt32, stride: UInt32, channels: UInt8, data:
/// List<sample>}`, row-major, packed (`data.len() == width * height *
/// channels`, no row padding — a producer using `stride` beyond that must
/// decode it itself; [`crate::tensor::ImageView`] does not). `sample` is
/// `UInt8`, `UInt16` or `Float32`, chosen by [`pixel_format_channels`].
///
/// `width`/`height`/`stride`/`channels` are stored as row data rather than
/// derived from the URN, even when the URN also names `width=`/`height=`,
/// because AstRS does not special-case "a fixed-resolution camera" as a
/// different layout — every `Image` row is self-describing, matching
/// `sensor_msgs/Image`'s own convention of carrying `width`/`height`/`step`
/// on every message even when they never change.
///
/// # Errors
///
/// Whatever [`pixel_format_channels`] reports.
///
/// ```
/// use astrs_data::urn::layouts::media::image_layout;
/// use astrs_data::DataType;
///
/// let layout = image_layout("rgb8")?;
/// let DataType::Struct(fields) = &layout else { unreachable!() };
/// assert_eq!(fields.iter().map(|f| f.name()).collect::<Vec<_>>(),
///            ["width", "height", "stride", "channels", "data"]);
/// # Ok::<(), astrs_data::TypeUrnError>(())
/// ```
pub fn image_layout(pixel: &str) -> Result<DataType, TypeUrnError> {
    let (sample_type, _channels) = pixel_format_channels(pixel)?;
    Ok(DataType::strukt([
        Field::required("width", DataType::UInt32),
        Field::required("height", DataType::UInt32),
        Field::required("stride", DataType::UInt32),
        Field::required("channels", DataType::UInt8),
        Field::required(
            "data",
            DataType::list(Field::required("sample", sample_type)),
        ),
    ]))
}

/// The single-column [`Schema`] for a `std/media/v1/Image[pixel=…]` payload.
///
/// # Errors
///
/// Whatever [`image_layout`] reports.
pub fn image_schema(pixel: &str) -> Result<Schema, TypeUrnError> {
    Ok(Schema::payload(image_layout(pixel)?, false))
}

/// [`LayoutResolver`](crate::urn::LayoutResolver) for `Image`, wired into the
/// registry.
///
/// # Errors
///
/// [`TypeUrnError::MissingParameter`] if called before the registry's
/// required-parameter check (defensive; the registry never does this in
/// practice), or whatever [`image_layout`] reports.
pub(crate) fn image_resolver(urn: &TypeUrn) -> Result<DataType, TypeUrnError> {
    let pixel = urn
        .param("pixel")
        .ok_or_else(|| TypeUrnError::MissingParameter {
            urn: STD_MEDIA_IMAGE.to_owned(),
            key: "pixel".to_owned(),
        })?;
    image_layout(pixel)
}

/// The columnar sample type a `std/media/v1/AudioFrame` `sample` value maps
/// onto.
///
/// # Errors
///
/// [`TypeUrnError::UnsupportedParameterValue`] for a `sample` string this
/// build does not map.
///
/// ```
/// use astrs_data::urn::layouts::media::sample_format_type;
/// use astrs_data::DataType;
///
/// assert_eq!(sample_format_type("s16"), Ok(DataType::Int16));
/// assert_eq!(sample_format_type("f32"), Ok(DataType::Float32));
/// assert!(sample_format_type("nope").is_err());
/// ```
pub fn sample_format_type(sample: &str) -> Result<DataType, TypeUrnError> {
    match sample {
        "u8" => Ok(DataType::UInt8),
        "s8" => Ok(DataType::Int8),
        "s16" => Ok(DataType::Int16),
        "s32" => Ok(DataType::Int32),
        "f32" => Ok(DataType::Float32),
        "f64" => Ok(DataType::Float64),
        other => Err(TypeUrnError::UnsupportedParameterValue {
            urn: STD_MEDIA_AUDIO_FRAME.to_owned(),
            key: "sample".to_owned(),
            value: other.to_owned(),
        }),
    }
}

/// `std/media/v1/AudioFrame[sample=…]` — a block of PCM audio samples.
///
/// `{sample_rate: UInt32, channels: UInt16, data: List<sample>}`, samples
/// interleaved across channels (`frame 0 ch 0, frame 0 ch 1, …, frame 1 ch
/// 0, …`) — the layout a PCM ring buffer already holds, so no de-interleave
/// step sits between capture and the wire. `sample` is chosen by
/// [`sample_format_type`]; `sample_rate`/`channels` are row data, like
/// [`image_layout`]'s `width`/`height`.
///
/// # Errors
///
/// Whatever [`sample_format_type`] reports.
pub fn audio_frame_layout(sample: &str) -> Result<DataType, TypeUrnError> {
    let sample_type = sample_format_type(sample)?;
    Ok(DataType::strukt([
        Field::required("sample_rate", DataType::UInt32),
        Field::required("channels", DataType::UInt16),
        Field::required(
            "data",
            DataType::list(Field::required("sample", sample_type)),
        ),
    ]))
}

/// The single-column [`Schema`] for a `std/media/v1/AudioFrame[sample=…]`
/// payload.
///
/// # Errors
///
/// Whatever [`audio_frame_layout`] reports.
pub fn audio_frame_schema(sample: &str) -> Result<Schema, TypeUrnError> {
    Ok(Schema::payload(audio_frame_layout(sample)?, false))
}

/// [`LayoutResolver`](crate::urn::LayoutResolver) for `AudioFrame`, wired
/// into the registry.
///
/// # Errors
///
/// [`TypeUrnError::MissingParameter`] if called before the registry's
/// required-parameter check (defensive; the registry never does this in
/// practice), or whatever [`audio_frame_layout`] reports.
pub(crate) fn audio_frame_resolver(urn: &TypeUrn) -> Result<DataType, TypeUrnError> {
    let sample = urn
        .param("sample")
        .ok_or_else(|| TypeUrnError::MissingParameter {
            urn: STD_MEDIA_AUDIO_FRAME.to_owned(),
            key: "sample".to_owned(),
        })?;
    audio_frame_layout(sample)
}

/// `std/media/v1/CompressedImage[format=…]` — an encoded image frame.
///
/// `{format: Utf8, data: Binary}`. Unlike `Image`/`AudioFrame`, the layout
/// does not depend on the *value* of `format` — every encoded stream is an
/// opaque byte blob regardless of codec — so `format` is required only for
/// the registry's parameter contract (a receiver must know *what* codec
/// produced `data`); the value itself is carried as row data instead of
/// baked into the type, so a decoder never needs the URN to interpret a
/// message it already has in hand.
#[must_use]
pub fn compressed_image_layout() -> DataType {
    DataType::strukt([
        Field::required("format", DataType::Utf8),
        Field::required("data", DataType::Binary),
    ])
}

/// The single-column [`Schema`] for a `std/media/v1/CompressedImage[format=…]`
/// payload.
#[must_use]
pub fn compressed_image_schema() -> Schema {
    Schema::payload(compressed_image_layout(), false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_documented_pixel_format_resolves() {
        for pixel in [
            "mono8",
            "mono16",
            "mono32f",
            "rgb8",
            "bgr8",
            "rgba8",
            "bgra8",
            "rgb16",
            "bgr16",
            "rgba16",
            "bgra16",
            "rgb32f",
            "rgba32f",
            "bayer_rggb8",
            "bayer_bggr8",
            "bayer_gbrg8",
            "bayer_grbg8",
        ] {
            assert!(pixel_format_channels(pixel).is_ok(), "{pixel}");
            assert!(image_layout(pixel).is_ok(), "{pixel}");
        }
    }

    #[test]
    fn image_layout_channel_count_matches_the_pixel_format() {
        let (_, channels) = pixel_format_channels("rgba16").unwrap();
        assert_eq!(channels, 4);
        let (sample_type, _) = pixel_format_channels("rgba16").unwrap();
        assert_eq!(sample_type, DataType::UInt16);
    }

    #[test]
    fn image_layout_is_the_same_shape_with_or_without_fixed_dimensions() {
        // The URN's width/height are graph-level hints; the columnar layout
        // is identical either way (see `image_layout`'s doc).
        assert_eq!(
            image_layout("mono8").unwrap(),
            image_layout("mono8").unwrap()
        );
    }

    #[test]
    fn image_resolver_reads_the_urn_parameter() {
        let urn = TypeUrn::parse("std/media/v1/Image[pixel=rgb8,width=640]").unwrap();
        assert_eq!(image_resolver(&urn), image_layout("rgb8"));

        let bad = TypeUrn::parse("std/media/v1/Image[pixel=nope]").unwrap();
        assert!(matches!(
            image_resolver(&bad),
            Err(TypeUrnError::UnsupportedParameterValue { .. })
        ));
    }

    #[test]
    fn every_documented_sample_format_resolves() {
        for sample in ["u8", "s8", "s16", "s32", "f32", "f64"] {
            assert!(sample_format_type(sample).is_ok(), "{sample}");
            assert!(audio_frame_layout(sample).is_ok(), "{sample}");
        }
        assert!(sample_format_type("s24").is_err(), "not in the table");
    }

    #[test]
    fn audio_frame_resolver_reads_the_urn_parameter() {
        let urn = TypeUrn::parse("std/media/v1/AudioFrame[sample=f32,rate=48000]").unwrap();
        assert_eq!(audio_frame_resolver(&urn), audio_frame_layout("f32"));
    }

    #[test]
    fn compressed_image_layout_does_not_depend_on_the_format_value() {
        assert_eq!(compressed_image_layout(), compressed_image_layout());
        let DataType::Struct(fields) = compressed_image_layout() else {
            panic!("expected a struct");
        };
        assert_eq!(fields[0].data_type(), &DataType::Utf8);
        assert_eq!(fields[1].data_type(), &DataType::Binary);
    }

    #[test]
    fn a_compressed_image_row_builds_and_reads_back() {
        use crate::array::{Array, BinaryArray, IntoArrayRef, StringArray, StructArray};

        let DataType::Struct(fields) = compressed_image_layout() else {
            panic!("expected a struct");
        };
        let row = StructArray::try_new(
            fields,
            vec![
                StringArray::from_values(["jpeg"]).into_array_ref(),
                BinaryArray::from_values([&b"\xff\xd8\xff"[..]]).into_array_ref(),
            ],
            None,
        )
        .unwrap();
        assert_eq!(row.len(), 1);
        assert_eq!(
            row.column_by_name("format")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .and_then(|a| a.get(0)),
            Some("jpeg")
        );
    }
}
