//! [`VisionError`] — everything an operation in this crate can fail with.
//!
//! One flat, `#[non_exhaustive]` enum covers the three failure families this
//! crate has: malformed image geometry (a buffer whose byte length does not
//! match its declared format/width/height), an operation applied to a pixel
//! format it does not support (YUYV handed to [`crate::sobel`], say), and a
//! malformed PNG stream. Keeping all three in one type — rather than one
//! error enum per module — is deliberate: an operator's `on_event` sees one
//! `Result` regardless of which stage inside it failed, and
//! [`astrs_operator_api::OpError::failed`] only wants a `Display`, not a
//! family of source types to match on.

use crate::pixel::PixelFormat;

/// The result type every fallible function in this crate returns.
pub type Result<T> = core::result::Result<T, VisionError>;

/// A failure from a buffer operation, a pixel-format conversion, a PNG codec
/// step, or a camera-model computation.
///
/// `#[non_exhaustive]`: new failure modes may join as this crate's op
/// coverage grows, matching the append-only evolution rule the rest of the
/// workspace's error types follow (see e.g. `astrs_operator_api::OpError`).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VisionError {
    /// A buffer's byte length does not match what `format`/`width`/`height`
    /// imply.
    #[error("a {width}x{height} {format} frame needs {expected} bytes, got {actual}")]
    BufferSizeMismatch {
        /// The declared pixel format.
        format: PixelFormat,
        /// The declared width, in pixels.
        width: u32,
        /// The declared height, in pixels.
        height: u32,
        /// The byte length the geometry implies.
        expected: usize,
        /// The byte length actually supplied.
        actual: usize,
    },

    /// A width/height pair cannot be represented at all — either it
    /// overflows the byte-count arithmetic, or (for a sub-sampled format
    /// such as [`PixelFormat::Yuyv`]) the width is not a multiple of the
    /// format's pixel-group size.
    #[error("{width}x{height} is not a valid {format} frame size: {reason}")]
    InvalidGeometry {
        /// The pixel format the size was checked against.
        format: PixelFormat,
        /// The rejected width.
        width: u32,
        /// The rejected height.
        height: u32,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// An operation does not support the pixel format it was handed.
    ///
    /// Most single-channel ops (threshold, Sobel, morphology, connected
    /// components) require [`PixelFormat::Mono8`]; most geometric/filtering
    /// ops (resize, blur, draw) require a fully-sampled format (anything but
    /// [`PixelFormat::Yuyv`]/[`PixelFormat::Uyvy`], whose two-pixel packing
    /// has no single-pixel byte range to copy or filter).
    #[error("{op} does not support pixel format {format}")]
    UnsupportedFormat {
        /// The operation that refused the format.
        op: &'static str,
        /// The format it was handed.
        format: PixelFormat,
    },

    /// A caller-supplied value did not carry exactly one item per channel of
    /// the image it was checked against — a [`crate::draw`] colour, most
    /// commonly.
    #[error("a {format} pixel needs {expected} values, got {actual}")]
    ChannelCountMismatch {
        /// The image format the value was checked against.
        format: PixelFormat,
        /// The channel count that format expects.
        expected: usize,
        /// The number of items the caller supplied.
        actual: usize,
    },

    /// A wire [`astrs_node_api::message::Image`] arrived in a sample width
    /// or pixel format this crate's 8-bit-only [`crate::ImageBuffer`] has no
    /// equivalent for (`mono16`, `rgb32f`, ...).
    #[error("astrs-nodes-vision only handles 8-bit images; wire pixel format {format} is not one")]
    UnsupportedWireFormat {
        /// The wire format that had no 8-bit crate equivalent.
        format: astrs_node_api::message::PixelFormat,
    },

    /// A requested output size, kernel, or structuring element has no valid
    /// interpretation (zero width, zero height, zero radius, ...).
    #[error("invalid {what}: {reason}")]
    InvalidParameter {
        /// The parameter that was rejected (`"resize target"`, `"gaussian
        /// sigma"`, ...).
        what: &'static str,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// Connected-component labeling found more components than a
    /// [`astrs_node_api::message::Mask`]'s `u16` label column can hold
    /// (label `0` is reserved for the background, leaving `u16::MAX`
    /// usable labels).
    #[error("connected components found {found} components, more than the {max} a Mask can label")]
    TooManyComponents {
        /// How many components labeling actually found.
        found: usize,
        /// The largest count a `Mask` can represent.
        max: usize,
    },

    /// A PNG stream failed to parse or decompress.
    #[error(transparent)]
    Png(#[from] crate::png::PngError),

    /// Building or reading an `astrs-data` columnar payload failed.
    #[error(transparent)]
    Data(#[from] astrs_data::DataError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn buffer_size_mismatch_names_every_dimension() {
        let error = VisionError::BufferSizeMismatch {
            format: PixelFormat::Rgb8,
            width: 2,
            height: 2,
            expected: 12,
            actual: 8,
        };
        let message = error.to_string();
        assert!(message.contains("2x2"));
        assert!(message.contains("rgb8"));
        assert!(message.contains('8'));
    }

    #[test]
    fn unsupported_format_names_the_operation() {
        let error = VisionError::UnsupportedFormat {
            op: "sobel",
            format: PixelFormat::Yuyv,
        };
        assert_eq!(
            error.to_string(),
            "sobel does not support pixel format yuyv"
        );
    }

    #[test]
    fn too_many_components_reports_both_counts() {
        let error = VisionError::TooManyComponents {
            found: 70_000,
            max: 65_535,
        };
        let message = error.to_string();
        assert!(message.contains("70000"));
        assert!(message.contains("65535"));
    }

    #[test]
    fn data_errors_convert_via_from() {
        let source = astrs_data::DataError::MessageRowCount { actual: 3 };
        let error: VisionError = source.into();
        assert!(matches!(error, VisionError::Data(_)));
    }
}
