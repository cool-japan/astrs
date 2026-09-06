//! `std/media/v1` — images and audio (§24.3).
//!
//! | Type | URN | Layout |
//! |---|---|---|
//! | [`Image`] | `Image[pixel=…]` | `{width, height, stride: UInt32, channels: UInt8, data: List<sample>}` |
//! | [`AudioFrame`] | `AudioFrame[sample=…]` | `{sample_rate: UInt32, channels: UInt16, data: List<sample>}` |
//! | [`CompressedImage`] | `CompressedImage[format=…]` | `{format: Utf8, data: Binary}` |
//!
//! [`Image`] and [`AudioFrame`] have a layout that *depends* on their URN
//! parameter — the sample column is `UInt8` for `rgb8` and `Float32` for
//! `rgb32f` — which a `const URN: &'static str` cannot express. Both carry
//! the parameter as a Rust value ([`PixelFormat`], [`SampleFormat`]) and
//! expose inherent `to_record_batch`/`from_record_batch` plus a `urn()` that
//! renders what a port should declare. [`CompressedImage`]'s layout does not
//! vary with its `format` (an encoded stream is opaque bytes whatever the
//! codec), so it implements [`AstrsMessage`] proper.
//!
//! Decoding an [`Image`] payload into `astrs-data`'s
//! [`astrs_data::tensor::ImageView`] — the shape blueprint §9.1
//! shows (`let img: ImageView = data.view()?;`) — works through this module's
//! [`super::FromPayload`] implementation for that type, which is
//! read-only because an `ImageView` is a checked view over columns somebody
//! else built.
//!
//! # Examples
//!
//! ```
//! use astrs_node_api::message::{Image, ImageSamples, PixelFormat};
//!
//! let frame = Image::new(PixelFormat::Mono8, 2, 2, ImageSamples::U8(vec![1, 2, 3, 4]))?;
//! assert_eq!(frame.urn(), "std/media/v1/Image[pixel=mono8]");
//! let batch = frame.to_record_batch()?;
//! assert_eq!(Image::from_record_batch(&batch)?, frame);
//! # Ok::<(), astrs_data::DataError>(())
//! ```

use astrs_data::tensor::ImageView;
use astrs_data::{AstrsMessage, DataError, DataType, Field, RecordBatch, Result};

use super::{FromPayload, build, read, single_row_column};

/// A pixel format from the §24.3 `Image[pixel=…]` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum PixelFormat {
    /// Single-channel 8-bit grey.
    Mono8,
    /// Single-channel 16-bit grey.
    Mono16,
    /// Single-channel 32-bit float grey.
    Mono32F,
    /// Three-channel 8-bit RGB.
    Rgb8,
    /// Three-channel 8-bit BGR.
    Bgr8,
    /// Four-channel 8-bit RGBA.
    Rgba8,
    /// Four-channel 8-bit BGRA.
    Bgra8,
    /// Three-channel 16-bit RGB.
    Rgb16,
    /// Four-channel 16-bit RGBA.
    Rgba16,
    /// Three-channel 32-bit float RGB.
    Rgb32F,
    /// Four-channel 32-bit float RGBA.
    Rgba32F,
}

impl PixelFormat {
    /// Every format, in documentation order.
    pub const ALL: &'static [Self] = &[
        Self::Mono8,
        Self::Mono16,
        Self::Mono32F,
        Self::Rgb8,
        Self::Bgr8,
        Self::Rgba8,
        Self::Bgra8,
        Self::Rgb16,
        Self::Rgba16,
        Self::Rgb32F,
        Self::Rgba32F,
    ];

    /// The `pixel=` parameter value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mono8 => "mono8",
            Self::Mono16 => "mono16",
            Self::Mono32F => "mono32f",
            Self::Rgb8 => "rgb8",
            Self::Bgr8 => "bgr8",
            Self::Rgba8 => "rgba8",
            Self::Bgra8 => "bgra8",
            Self::Rgb16 => "rgb16",
            Self::Rgba16 => "rgba16",
            Self::Rgb32F => "rgb32f",
            Self::Rgba32F => "rgba32f",
        }
    }

    /// The format a `pixel=` value names.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|format| format.as_str() == value)
    }

    /// How many channels one pixel holds.
    #[must_use]
    pub const fn channels(self) -> u8 {
        match self {
            Self::Mono8 | Self::Mono16 | Self::Mono32F => 1,
            Self::Rgb8 | Self::Bgr8 | Self::Rgb16 | Self::Rgb32F => 3,
            Self::Rgba8 | Self::Bgra8 | Self::Rgba16 | Self::Rgba32F => 4,
        }
    }

    /// The columnar type one sample occupies.
    #[must_use]
    pub const fn sample_type(self) -> DataType {
        match self {
            Self::Mono8 | Self::Rgb8 | Self::Bgr8 | Self::Rgba8 | Self::Bgra8 => DataType::UInt8,
            Self::Mono16 | Self::Rgb16 | Self::Rgba16 => DataType::UInt16,
            Self::Mono32F | Self::Rgb32F | Self::Rgba32F => DataType::Float32,
        }
    }

    /// The parameterised URN a port carrying this format declares.
    #[must_use]
    pub fn urn(self) -> String {
        format!("std/media/v1/Image[pixel={}]", self.as_str())
    }

    /// The columnar layout an image in this format produces.
    #[must_use]
    pub fn layout(self) -> DataType {
        DataType::strukt([
            Field::required("width", DataType::UInt32),
            Field::required("height", DataType::UInt32),
            Field::required("stride", DataType::UInt32),
            Field::required("channels", DataType::UInt8),
            Field::required(
                "data",
                DataType::list(Field::required("sample", self.sample_type())),
            ),
        ])
    }
}

impl core::fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An image's samples, in whichever width its [`PixelFormat`] selects.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ImageSamples {
    /// 8-bit samples.
    U8(Vec<u8>),
    /// 16-bit samples.
    U16(Vec<u16>),
    /// 32-bit float samples.
    F32(Vec<f32>),
}

impl ImageSamples {
    /// How many samples the buffer holds.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::U8(values) => values.len(),
            Self::U16(values) => values.len(),
            Self::F32(values) => values.len(),
        }
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The columnar type these samples occupy.
    #[must_use]
    pub const fn sample_type(&self) -> DataType {
        match self {
            Self::U8(_) => DataType::UInt8,
            Self::U16(_) => DataType::UInt16,
            Self::F32(_) => DataType::Float32,
        }
    }
}

/// A raw image frame — `std/media/v1/Image[pixel=…]`, row-major and packed.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    /// The pixel format, which selects the sample column's type.
    pub pixel: PixelFormat,
    /// The frame's width in pixels.
    pub width: u32,
    /// The frame's height in pixels.
    pub height: u32,
    /// The bytes between the starts of two rows; equal to the packed row
    /// length for a frame with no padding, which is what this layout carries.
    pub stride: u32,
    /// The samples, `width * height * channels` of them.
    pub data: ImageSamples,
}

impl Image {
    /// A packed frame whose sample count matches its geometry.
    ///
    /// `stride` is computed as the packed row length in *bytes*.
    ///
    /// # Errors
    ///
    /// [`DataError::TypeMismatch`] when the samples do not match the format's
    /// width, and [`DataError::ChildLengthMismatch`] when the sample count is
    /// not `width * height * channels`.
    pub fn new(pixel: PixelFormat, width: u32, height: u32, data: ImageSamples) -> Result<Self> {
        let stride = packed_stride(pixel, width);
        let value = Self {
            pixel,
            width,
            height,
            stride,
            data,
        };
        value.check()?;
        Ok(value)
    }

    /// How many channels one pixel holds.
    #[must_use]
    pub const fn channels(&self) -> u8 {
        self.pixel.channels()
    }

    /// The parameterised URN a port carrying this frame declares.
    #[must_use]
    pub fn urn(&self) -> String {
        self.pixel.urn()
    }

    /// The columnar layout this frame produces.
    #[must_use]
    pub fn layout(&self) -> DataType {
        self.pixel.layout()
    }

    /// Checks that the samples match the format and the geometry.
    ///
    /// # Errors
    ///
    /// As [`Image::new`].
    pub fn check(&self) -> Result<()> {
        if self.data.sample_type() != self.pixel.sample_type() {
            return Err(DataError::type_mismatch(
                self.pixel.sample_type(),
                self.data.sample_type(),
            ));
        }
        let expected = expected_samples(self.width, self.height, self.channels());
        if self.data.len() != expected {
            return Err(DataError::ChildLengthMismatch {
                expected,
                actual: self.data.len(),
            });
        }
        Ok(())
    }

    /// Encodes the frame as a one-row payload batch.
    ///
    /// # Errors
    ///
    /// As [`Image::check`], plus [`DataError`] when the columns cannot be
    /// assembled.
    pub fn to_record_batch(&self) -> Result<RecordBatch> {
        self.check()?;
        let samples = match &self.data {
            ImageSamples::U8(values) => build::primitive_lists::<u8>("sample", &[values])?,
            ImageSamples::U16(values) => build::primitive_lists::<u16>("sample", &[values])?,
            ImageSamples::F32(values) => build::primitive_lists::<f32>("sample", &[values])?,
        };
        let column = build::structure(vec![
            ("width", build::primitive::<u32>(&[self.width])),
            ("height", build::primitive::<u32>(&[self.height])),
            ("stride", build::primitive::<u32>(&[self.stride])),
            ("channels", build::primitive::<u8>(&[self.pixel.channels()])),
            ("data", samples),
        ])?;
        Ok(RecordBatch::from_payload(column))
    }

    /// Decodes a frame from a one-row payload batch.
    ///
    /// The pixel format is recovered from the sample column's type and the
    /// stored channel count, so a decoder never needs the port's URN to read
    /// a frame it already holds.
    ///
    /// # Errors
    ///
    /// [`DataError`] when the batch is not an `Image` layout, or its sample
    /// type and channel count name no format in the §24.3 table.
    pub fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        let width = read::primitive_at::<u32>(read::child(column, "width")?, 0)?;
        let height = read::primitive_at::<u32>(read::child(column, "height")?, 0)?;
        let stride = read::primitive_at::<u32>(read::child(column, "stride")?, 0)?;
        let channels = read::primitive_at::<u8>(read::child(column, "channels")?, 0)?;
        let data_column = read::child(column, "data")?;
        let sample_type = match data_column.data_type() {
            DataType::List(field) => field.data_type().clone(),
            other => {
                return Err(DataError::type_mismatch(
                    DataType::list(Field::required("sample", DataType::UInt8)),
                    other.clone(),
                ));
            }
        };
        let data = match sample_type {
            DataType::UInt8 => ImageSamples::U8(read::primitive_list_at::<u8>(data_column, 0)?),
            DataType::UInt16 => ImageSamples::U16(read::primitive_list_at::<u16>(data_column, 0)?),
            DataType::Float32 => ImageSamples::F32(read::primitive_list_at::<f32>(data_column, 0)?),
            other => return Err(DataError::type_mismatch(DataType::UInt8, other)),
        };
        let pixel = pixel_for(&data.sample_type(), channels)
            .ok_or_else(|| DataError::type_mismatch(DataType::UInt8, data.sample_type()))?;
        let value = Self {
            pixel,
            width,
            height,
            stride,
            data,
        };
        value.check()?;
        Ok(value)
    }
}

impl FromPayload for Image {
    fn from_batch(batch: &RecordBatch) -> Result<Self> {
        Self::from_record_batch(batch)
    }
}

impl FromPayload for ImageView {
    /// Reads an `std/media/v1/Image` payload as a checked N-D view.
    ///
    /// This is the blueprint §9.1 shape: `let img: ImageView = data.view()?;`
    /// — a *view*, not a copy, over the payload's own sample column.
    fn from_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        let strukt = read::structure(column)?;
        Self::from_struct_row(strukt, 0)
    }
}

/// An audio sample format from the §24.3 `AudioFrame[sample=…]` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum SampleFormat {
    /// Unsigned 8-bit PCM.
    U8,
    /// Signed 8-bit PCM.
    S8,
    /// Signed 16-bit PCM.
    S16,
    /// Signed 32-bit PCM.
    S32,
    /// 32-bit float PCM.
    F32,
    /// 64-bit float PCM.
    F64,
}

impl SampleFormat {
    /// Every format, in documentation order.
    pub const ALL: &'static [Self] = &[
        Self::U8,
        Self::S8,
        Self::S16,
        Self::S32,
        Self::F32,
        Self::F64,
    ];

    /// The `sample=` parameter value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::U8 => "u8",
            Self::S8 => "s8",
            Self::S16 => "s16",
            Self::S32 => "s32",
            Self::F32 => "f32",
            Self::F64 => "f64",
        }
    }

    /// The format a `sample=` value names.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|format| format.as_str() == value)
    }

    /// The columnar type one sample occupies.
    #[must_use]
    pub const fn sample_type(self) -> DataType {
        match self {
            Self::U8 => DataType::UInt8,
            Self::S8 => DataType::Int8,
            Self::S16 => DataType::Int16,
            Self::S32 => DataType::Int32,
            Self::F32 => DataType::Float32,
            Self::F64 => DataType::Float64,
        }
    }

    /// The parameterised URN a port carrying this format declares.
    #[must_use]
    pub fn urn(self) -> String {
        format!("std/media/v1/AudioFrame[sample={}]", self.as_str())
    }

    /// The columnar layout an audio frame in this format produces.
    #[must_use]
    pub fn layout(self) -> DataType {
        DataType::strukt([
            Field::required("sample_rate", DataType::UInt32),
            Field::required("channels", DataType::UInt16),
            Field::required(
                "data",
                DataType::list(Field::required("sample", self.sample_type())),
            ),
        ])
    }
}

impl core::fmt::Display for SampleFormat {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A block of interleaved PCM audio — `std/media/v1/AudioFrame[sample=…]`.
///
/// Samples are interleaved across channels (`frame 0 ch 0, frame 0 ch 1, …`),
/// which is the layout a capture ring buffer already holds.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioFrame {
    /// The sample format, which selects the sample column's type.
    pub format: SampleFormat,
    /// Frames per second.
    pub sample_rate: u32,
    /// How many channels are interleaved.
    pub channels: u16,
    /// The interleaved samples, as 32-bit floats regardless of the wire
    /// format — the one shape every audio pipeline can consume, converted on
    /// encode.
    pub data: Vec<f32>,
}

impl AudioFrame {
    /// A frame of interleaved `f32` samples.
    ///
    /// # Errors
    ///
    /// [`DataError::ChildLengthMismatch`] when the sample count is not a
    /// whole number of interleaved frames.
    pub fn new(
        format: SampleFormat,
        sample_rate: u32,
        channels: u16,
        data: Vec<f32>,
    ) -> Result<Self> {
        let value = Self {
            format,
            sample_rate,
            channels,
            data,
        };
        value.check()?;
        Ok(value)
    }

    /// How many interleaved frames the block holds.
    #[must_use]
    pub fn frame_count(&self) -> usize {
        let channels = usize::from(self.channels.max(1));
        self.data.len() / channels
    }

    /// The parameterised URN a port carrying this frame declares.
    #[must_use]
    pub fn urn(&self) -> String {
        self.format.urn()
    }

    /// The columnar layout this frame produces.
    #[must_use]
    pub fn layout(&self) -> DataType {
        self.format.layout()
    }

    /// Checks that the sample count is a whole number of frames.
    ///
    /// # Errors
    ///
    /// [`DataError::ChildLengthMismatch`].
    pub fn check(&self) -> Result<()> {
        let channels = usize::from(self.channels.max(1));
        if self.data.len().is_multiple_of(channels) {
            Ok(())
        } else {
            Err(DataError::ChildLengthMismatch {
                expected: self.data.len().next_multiple_of(channels),
                actual: self.data.len(),
            })
        }
    }

    /// Encodes the frame as a one-row payload batch, converting the samples
    /// into the format's own column type.
    ///
    /// # Errors
    ///
    /// As [`AudioFrame::check`], plus [`DataError`] when the columns cannot be
    /// assembled.
    pub fn to_record_batch(&self) -> Result<RecordBatch> {
        self.check()?;
        let samples = match self.format {
            SampleFormat::U8 => build::primitive_lists::<u8>(
                "sample",
                &[&self.data.iter().map(|v| clamp_u8(*v)).collect::<Vec<u8>>()],
            )?,
            SampleFormat::S8 => build::primitive_lists::<i8>(
                "sample",
                &[&self.data.iter().map(|v| clamp_i8(*v)).collect::<Vec<i8>>()],
            )?,
            SampleFormat::S16 => build::primitive_lists::<i16>(
                "sample",
                &[&self
                    .data
                    .iter()
                    .map(|v| clamp_i16(*v))
                    .collect::<Vec<i16>>()],
            )?,
            SampleFormat::S32 => build::primitive_lists::<i32>(
                "sample",
                &[&self
                    .data
                    .iter()
                    .map(|v| clamp_i32(*v))
                    .collect::<Vec<i32>>()],
            )?,
            SampleFormat::F32 => build::primitive_lists::<f32>("sample", &[&self.data])?,
            SampleFormat::F64 => build::primitive_lists::<f64>(
                "sample",
                &[&self
                    .data
                    .iter()
                    .map(|v| f64::from(*v))
                    .collect::<Vec<f64>>()],
            )?,
        };
        let column = build::structure(vec![
            ("sample_rate", build::primitive::<u32>(&[self.sample_rate])),
            ("channels", build::primitive::<u16>(&[self.channels])),
            ("data", samples),
        ])?;
        Ok(RecordBatch::from_payload(column))
    }

    /// Decodes a frame from a one-row payload batch, converting the samples
    /// back into `f32`.
    ///
    /// # Errors
    ///
    /// [`DataError`] when the batch is not an `AudioFrame` layout.
    pub fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        let sample_rate = read::primitive_at::<u32>(read::child(column, "sample_rate")?, 0)?;
        let channels = read::primitive_at::<u16>(read::child(column, "channels")?, 0)?;
        let data_column = read::child(column, "data")?;
        let sample_type = match data_column.data_type() {
            DataType::List(field) => field.data_type().clone(),
            other => {
                return Err(DataError::type_mismatch(
                    DataType::list(Field::required("sample", DataType::Float32)),
                    other.clone(),
                ));
            }
        };
        let (format, data) = match sample_type {
            DataType::UInt8 => (
                SampleFormat::U8,
                read::primitive_list_at::<u8>(data_column, 0)?
                    .into_iter()
                    .map(narrow_u8)
                    .collect(),
            ),
            DataType::Int8 => (
                SampleFormat::S8,
                read::primitive_list_at::<i8>(data_column, 0)?
                    .into_iter()
                    .map(narrow_i8)
                    .collect(),
            ),
            DataType::Int16 => (
                SampleFormat::S16,
                read::primitive_list_at::<i16>(data_column, 0)?
                    .into_iter()
                    .map(narrow_i16)
                    .collect(),
            ),
            DataType::Int32 => (
                SampleFormat::S32,
                read::primitive_list_at::<i32>(data_column, 0)?
                    .into_iter()
                    .map(narrow_i32)
                    .collect(),
            ),
            DataType::Float32 => (
                SampleFormat::F32,
                read::primitive_list_at::<f32>(data_column, 0)?,
            ),
            DataType::Float64 => (
                SampleFormat::F64,
                read::primitive_list_at::<f64>(data_column, 0)?
                    .into_iter()
                    .map(narrow_f64)
                    .collect(),
            ),
            other => return Err(DataError::type_mismatch(DataType::Float32, other)),
        };
        Self::new(format, sample_rate, channels, data)
    }
}

impl FromPayload for AudioFrame {
    fn from_batch(batch: &RecordBatch) -> Result<Self> {
        Self::from_record_batch(batch)
    }
}

/// An encoded image frame — `std/media/v1/CompressedImage[format=…]`.
///
/// The only parameterised media type whose *layout* does not vary with its
/// parameter: an encoded stream is opaque bytes whatever the codec, and the
/// codec name rides as row data so a decoder never needs the port's URN.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompressedImage {
    /// The codec that produced `data` (`jpeg`, `png`, `h264`, …).
    pub format: String,
    /// The encoded bytes.
    pub data: Vec<u8>,
}

impl CompressedImage {
    /// A frame from its codec name and bytes.
    #[must_use]
    pub fn new(format: impl Into<String>, data: Vec<u8>) -> Self {
        Self {
            format: format.into(),
            data,
        }
    }

    /// The parameterised URN a port carrying this frame declares.
    #[must_use]
    pub fn urn(&self) -> String {
        format!("std/media/v1/CompressedImage[format={}]", self.format)
    }

    /// The columnar layout of this type.
    #[must_use]
    pub fn layout() -> DataType {
        DataType::strukt([
            Field::required("format", DataType::Utf8),
            Field::required("data", DataType::Binary),
        ])
    }
}

impl AstrsMessage for CompressedImage {
    const URN: &'static str = "std/media/v1/CompressedImage";

    fn data_type() -> DataType {
        Self::layout()
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        let column = build::structure(vec![
            ("format", build::strings(&[&self.format])),
            ("data", build::binaries(&[&self.data])),
        ])?;
        Ok(RecordBatch::from_payload(column))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        Ok(Self {
            format: read::str_at(read::child(column, "format")?, 0)?.to_owned(),
            data: read::bytes_at(read::child(column, "data")?, 0)?.to_vec(),
        })
    }
}

super::impl_from_payload!(CompressedImage);

/// The packed row length in bytes for a format and width.
fn packed_stride(pixel: PixelFormat, width: u32) -> u32 {
    let sample_width = match pixel.sample_type() {
        DataType::UInt8 => 1u64,
        DataType::UInt16 => 2,
        _ => 4,
    };
    let bytes = u64::from(width) * u64::from(pixel.channels()) * sample_width;
    u32::try_from(bytes).unwrap_or(u32::MAX)
}

/// The sample count a geometry implies, saturating rather than wrapping.
fn expected_samples(width: u32, height: u32, channels: u8) -> usize {
    let total = u64::from(width) * u64::from(height) * u64::from(channels);
    usize::try_from(total).unwrap_or(usize::MAX)
}

/// The format a `(sample type, channel count)` pair names, if any.
fn pixel_for(sample_type: &DataType, channels: u8) -> Option<PixelFormat> {
    PixelFormat::ALL
        .iter()
        .copied()
        .find(|format| &format.sample_type() == sample_type && format.channels() == channels)
}

/// Scales a normalised `f32` sample into unsigned 8-bit PCM.
fn clamp_u8(value: f32) -> u8 {
    let scaled = (value.clamp(-1.0, 1.0) * 127.0) + 128.0;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the value is clamped into 1.0..=255.0 before the cast"
    )]
    {
        scaled.round().clamp(0.0, 255.0) as u8
    }
}

/// Scales a normalised `f32` sample into signed 8-bit PCM.
fn clamp_i8(value: f32) -> i8 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the value is clamped into i8's range before the cast"
    )]
    {
        (value.clamp(-1.0, 1.0) * 127.0)
            .round()
            .clamp(-128.0, 127.0) as i8
    }
}

/// Scales a normalised `f32` sample into signed 16-bit PCM.
fn clamp_i16(value: f32) -> i16 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the value is clamped into i16's range before the cast"
    )]
    {
        (value.clamp(-1.0, 1.0) * 32_767.0)
            .round()
            .clamp(-32_768.0, 32_767.0) as i16
    }
}

/// Scales a normalised `f32` sample into signed 32-bit PCM.
fn clamp_i32(value: f32) -> i32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the value is clamped into i32's range before the cast"
    )]
    {
        (f64::from(value.clamp(-1.0, 1.0)) * 2_147_483_647.0)
            .round()
            .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32
    }
}

/// Scales unsigned 8-bit PCM back into a normalised `f32`, undoing
/// [`clamp_u8`]'s mid-scale offset.
fn narrow_u8(value: u8) -> f32 {
    (f32::from(value) - 128.0) / 127.0
}

/// Scales signed 8-bit PCM back into a normalised `f32`.
fn narrow_i8(value: i8) -> f32 {
    f32::from(value) / 127.0
}

/// Scales signed 16-bit PCM back into a normalised `f32`.
fn narrow_i16(value: i16) -> f32 {
    f32::from(value) / 32_767.0
}

/// Scales signed 32-bit PCM back into a normalised `f32`.
fn narrow_i32(value: i32) -> f32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a normalised ratio is well inside f32's range"
    )]
    {
        (f64::from(value) / 2_147_483_647.0) as f32
    }
}

/// Narrows a `f64` sample to `f32`.
fn narrow_f64(value: f64) -> f32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "PCM samples are normalised, so the narrowing is lossless in practice"
    )]
    {
        value as f32
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::message::{assert_layout_matches, assert_registry_layout_for};

    #[test]
    fn every_pixel_format_layout_matches_the_registry() {
        for format in PixelFormat::ALL {
            assert_layout_matches(&format.urn(), &format.layout()).unwrap();
            assert_eq!(PixelFormat::parse(format.as_str()), Some(*format));
            assert_eq!(format.to_string(), format.as_str());
        }
        assert_eq!(PixelFormat::parse("nope"), None);
    }

    #[test]
    fn every_sample_format_layout_matches_the_registry() {
        for format in SampleFormat::ALL {
            assert_layout_matches(&format.urn(), &format.layout()).unwrap();
            assert_eq!(SampleFormat::parse(format.as_str()), Some(*format));
            assert_eq!(format.to_string(), format.as_str());
        }
        assert_eq!(SampleFormat::parse("nope"), None);
    }

    #[test]
    fn compressed_images_conform_and_round_trip() {
        assert_registry_layout_for::<CompressedImage>("std/media/v1/CompressedImage[format=jpeg]")
            .unwrap();
        let frame = CompressedImage::new("jpeg", vec![0xFF, 0xD8, 0xFF]);
        let batch = frame.to_record_batch().unwrap();
        assert_eq!(CompressedImage::from_record_batch(&batch).unwrap(), frame);
        assert_eq!(frame.urn(), "std/media/v1/CompressedImage[format=jpeg]");
        assert_eq!(CompressedImage::default().data.len(), 0);
    }

    #[test]
    fn images_round_trip_in_every_sample_width() {
        let cases = [
            (
                PixelFormat::Mono8,
                ImageSamples::U8(vec![1, 2, 3, 4]),
                2u32,
                2u32,
            ),
            (
                PixelFormat::Mono16,
                ImageSamples::U16(vec![1, 2, 3, 4]),
                2,
                2,
            ),
            (PixelFormat::Rgb32F, ImageSamples::F32(vec![0.0; 12]), 2, 2),
        ];
        for (pixel, samples, width, height) in cases {
            let frame = Image::new(pixel, width, height, samples).unwrap();
            assert_layout_matches(&frame.urn(), &frame.layout()).unwrap();
            let batch = frame.to_record_batch().unwrap();
            let decoded = Image::from_record_batch(&batch).unwrap();
            assert_eq!(decoded, frame, "{pixel}");
            assert_eq!(decoded.channels(), pixel.channels());
        }
    }

    #[test]
    fn an_image_view_reads_the_same_payload() {
        let frame =
            Image::new(PixelFormat::Rgb8, 2, 2, ImageSamples::U8((0..12).collect())).unwrap();
        let batch = frame.to_record_batch().unwrap();
        let view = <ImageView as FromPayload>::from_batch(&batch).unwrap();
        assert_eq!(view.width(), 2);
        assert_eq!(view.height(), 2);
        assert_eq!(view.channels(), 3);
        assert_eq!(view.get_f64(&[0, 0, 0]).unwrap(), 0.0);
    }

    #[test]
    fn a_mismatched_image_is_refused() {
        let error = Image::new(PixelFormat::Mono8, 2, 2, ImageSamples::U8(vec![1, 2])).unwrap_err();
        assert!(matches!(error, DataError::ChildLengthMismatch { .. }));

        let error = Image::new(PixelFormat::Mono8, 1, 1, ImageSamples::F32(vec![1.0])).unwrap_err();
        assert!(matches!(error, DataError::TypeMismatch { .. }));

        assert!(ImageSamples::U8(Vec::new()).is_empty());
        assert_eq!(ImageSamples::U16(vec![1, 2]).len(), 2);
    }

    #[test]
    fn audio_frames_round_trip_through_every_sample_format() {
        for format in SampleFormat::ALL {
            let frame = AudioFrame::new(*format, 48_000, 2, vec![0.0, 0.5, -0.5, 1.0]).unwrap();
            let batch = frame.to_record_batch().unwrap();
            let decoded = AudioFrame::from_record_batch(&batch).unwrap();
            assert_eq!(decoded.format, *format);
            assert_eq!(decoded.sample_rate, 48_000);
            assert_eq!(decoded.channels, 2);
            assert_eq!(decoded.frame_count(), 2);
            for (original, round_tripped) in frame.data.iter().zip(decoded.data.iter()) {
                assert!(
                    (original - round_tripped).abs() < 0.02,
                    "{format}: {original} vs {round_tripped}"
                );
            }
        }
    }

    #[test]
    fn an_audio_frame_with_a_partial_interleave_is_refused() {
        let error = AudioFrame::new(SampleFormat::F32, 48_000, 3, vec![0.0, 0.5]).unwrap_err();
        assert!(matches!(error, DataError::ChildLengthMismatch { .. }));
    }

    #[test]
    fn a_wrong_layout_is_refused() {
        let compressed = CompressedImage::new("png", vec![1])
            .to_record_batch()
            .unwrap();
        assert!(Image::from_record_batch(&compressed).is_err());
        assert!(AudioFrame::from_record_batch(&compressed).is_err());
        assert!(<ImageView as FromPayload>::from_batch(&compressed).is_err());

        let image = Image::new(PixelFormat::Mono8, 1, 1, ImageSamples::U8(vec![7]))
            .unwrap()
            .to_record_batch()
            .unwrap();
        assert!(CompressedImage::from_record_batch(&image).is_err());
    }

    #[test]
    fn strides_are_the_packed_row_length_in_bytes() {
        assert_eq!(packed_stride(PixelFormat::Mono8, 640), 640);
        assert_eq!(packed_stride(PixelFormat::Rgb8, 640), 1_920);
        assert_eq!(packed_stride(PixelFormat::Rgba16, 640), 5_120);
        assert_eq!(packed_stride(PixelFormat::Rgb32F, 640), 7_680);
    }

    #[test]
    fn pixel_formats_are_recovered_from_their_columns() {
        assert_eq!(pixel_for(&DataType::UInt8, 1), Some(PixelFormat::Mono8));
        assert_eq!(pixel_for(&DataType::UInt8, 3), Some(PixelFormat::Rgb8));
        assert_eq!(pixel_for(&DataType::Float32, 4), Some(PixelFormat::Rgba32F));
        assert_eq!(pixel_for(&DataType::Int8, 1), None);
        assert_eq!(pixel_for(&DataType::UInt8, 7), None);
    }
}
