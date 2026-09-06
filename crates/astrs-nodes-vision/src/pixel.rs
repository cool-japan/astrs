//! [`PixelFormat`] — the byte layout of one image payload.
//!
//! Camera frames arrive in whatever format the driver produces. This module
//! is the vocabulary every other module in this crate shares for naming that
//! layout, and the arithmetic ([`PixelFormat::frame_bytes`]) for checking a
//! buffer against it without ever risking the overflow or silent rounding a
//! hand-written `width * height * bytes_per_pixel` would.
//!
//! ```
//! use astrs_nodes_vision::PixelFormat;
//!
//! // Frame sizing is exact, and overflow-checked rather than wrapping.
//! assert_eq!(PixelFormat::Rgb8.frame_bytes(1920, 1080), Some(6_220_800));
//!
//! // Packed YUYV stores two pixels per four-byte group, so an odd width
//! // simply has no valid packing — that is `None`, not a rounded-up guess.
//! assert_eq!(PixelFormat::Yuyv.frame_bytes(641, 480), None);
//! assert_eq!(PixelFormat::Yuyv.frame_bytes(640, 480), Some(614_400));
//! ```

/// The byte layout of one image payload.
///
/// Every variant is a packed, 8-bit-per-component format: these are what
/// cameras actually deliver and what encoders actually consume. Planar and
/// higher-depth formats are converted at the driver edge rather than modelled
/// here.
///
/// [`PixelFormat::Yuyv`] and [`PixelFormat::Uyvy`] are the two packed 4:2:2
/// byte orders real hardware ships (USB/UVC webcams typically emit `YUYV`;
/// many GigE/machine-vision cameras emit `UYVY`) — same subsampling, same
/// four bytes per pixel pair, differing only in which byte comes first. Both
/// are converted to a fully-sampled format ([`crate::color`]) before any op
/// in this crate other than [`crate::color::yuv422_to_rgb8`] and its
/// siblings will touch them; see [`PixelFormat::is_fully_sampled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PixelFormat {
    /// Single-channel 8-bit greyscale.
    Mono8,
    /// Three 8-bit channels in red, green, blue order.
    Rgb8,
    /// Three 8-bit channels in blue, green, red order — what most USB and
    /// GigE cameras hand out.
    Bgr8,
    /// Four 8-bit channels in red, green, blue, alpha order.
    Rgba8,
    /// Packed 4:2:2 YUV, byte order `Y0 U Y1 V`: two pixels share one chroma
    /// pair across four bytes, so a row must contain an even number of
    /// pixels. The common USB/UVC webcam byte order.
    Yuyv,
    /// Packed 4:2:2 YUV, byte order `U Y0 V Y1` — the same subsampling as
    /// [`PixelFormat::Yuyv`] with chroma and luma swapped in the byte
    /// stream. Common on GigE/machine-vision cameras.
    Uyvy,
}

impl PixelFormat {
    /// Every format, in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Mono8,
        Self::Rgb8,
        Self::Bgr8,
        Self::Rgba8,
        Self::Yuyv,
        Self::Uyvy,
    ];

    /// How many colour channels this format carries.
    ///
    /// For [`PixelFormat::Yuyv`]/[`PixelFormat::Uyvy`] this is the *logical*
    /// channel count (luma plus two chroma); the chroma channels are shared
    /// between neighbouring pixels, which is why
    /// [`PixelFormat::bytes_per_pixel`] is `2` rather than `3`.
    #[must_use]
    pub const fn channels(self) -> usize {
        match self {
            Self::Mono8 => 1,
            Self::Rgb8 | Self::Bgr8 | Self::Yuyv | Self::Uyvy => 3,
            Self::Rgba8 => 4,
        }
    }

    /// The average number of stored bytes per pixel.
    #[must_use]
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Mono8 => 1,
            Self::Yuyv | Self::Uyvy => 2,
            Self::Rgb8 | Self::Bgr8 => 3,
            Self::Rgba8 => 4,
        }
    }

    /// Whether this format carries an alpha channel.
    #[must_use]
    pub const fn has_alpha(self) -> bool {
        matches!(self, Self::Rgba8)
    }

    /// How many pixels one packing group covers — `2` for the subsampled
    /// [`PixelFormat::Yuyv`]/[`PixelFormat::Uyvy`], `1` for every
    /// fully-sampled format.
    ///
    /// A frame's width must be a multiple of this for the row to be
    /// representable at all.
    #[must_use]
    pub const fn pixels_per_group(self) -> usize {
        match self {
            Self::Yuyv | Self::Uyvy => 2,
            Self::Mono8 | Self::Rgb8 | Self::Bgr8 | Self::Rgba8 => 1,
        }
    }

    /// Whether every pixel owns its own whole byte range — `false` only for
    /// the packed 4:2:2 formats, whose chroma bytes are shared between two
    /// pixels.
    ///
    /// Every per-pixel op in this crate (resize, blur, threshold, drawing,
    /// ...) requires this; convert with [`crate::color`] first.
    #[must_use]
    pub const fn is_fully_sampled(self) -> bool {
        self.pixels_per_group() == 1
    }

    /// The exact byte length of a tightly-packed `width`×`height` frame.
    ///
    /// [`None`] when the frame cannot be represented in this format at all:
    /// a `width` that is not a multiple of [`PixelFormat::pixels_per_group`]
    /// (an odd-width YUYV row has no valid packing), or a size whose byte
    /// count overflows [`usize`]. Both are reported rather than rounded or
    /// wrapped — a silently wrong buffer length is a memory-safety bug
    /// waiting for its first `unsafe` block.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_nodes_vision::PixelFormat;
    ///
    /// assert_eq!(PixelFormat::Mono8.frame_bytes(4, 4), Some(16));
    /// assert_eq!(PixelFormat::Rgba8.frame_bytes(0, 0), Some(0));
    /// assert_eq!(PixelFormat::Yuyv.frame_bytes(3, 1), None);
    /// ```
    #[must_use]
    pub const fn frame_bytes(self, width: usize, height: usize) -> Option<usize> {
        if !width.is_multiple_of(self.pixels_per_group()) {
            return None;
        }
        let Some(pixels) = width.checked_mul(height) else {
            return None;
        };
        pixels.checked_mul(self.bytes_per_pixel())
    }

    /// The exact byte length of one tightly-packed row of `width` pixels,
    /// with the same [`None`] conditions as [`PixelFormat::frame_bytes`].
    #[must_use]
    pub const fn row_bytes(self, width: usize) -> Option<usize> {
        self.frame_bytes(width, 1)
    }

    /// The format named by a `pixel=` string, matching
    /// [`PixelFormat::as_str`]'s spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|f| f.as_str() == value)
    }

    /// The lowercase, ROS-`sensor_msgs`-style encoding name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mono8 => "mono8",
            Self::Rgb8 => "rgb8",
            Self::Bgr8 => "bgr8",
            Self::Rgba8 => "rgba8",
            Self::Yuyv => "yuyv",
            Self::Uyvy => "uyvy",
        }
    }
}

impl std::fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    const EVERY_FORMAT: &[PixelFormat] = PixelFormat::ALL;

    #[test]
    fn byte_counts_match_the_packed_layouts() {
        assert_eq!(PixelFormat::Mono8.bytes_per_pixel(), 1);
        assert_eq!(PixelFormat::Yuyv.bytes_per_pixel(), 2);
        assert_eq!(PixelFormat::Uyvy.bytes_per_pixel(), 2);
        assert_eq!(PixelFormat::Rgb8.bytes_per_pixel(), 3);
        assert_eq!(PixelFormat::Bgr8.bytes_per_pixel(), 3);
        assert_eq!(PixelFormat::Rgba8.bytes_per_pixel(), 4);
    }

    #[test]
    fn only_rgba_carries_alpha() {
        for format in EVERY_FORMAT {
            assert_eq!(
                format.has_alpha(),
                *format == PixelFormat::Rgba8,
                "{format}"
            );
        }
    }

    #[test]
    fn only_the_yuv422_pair_is_not_fully_sampled() {
        for format in EVERY_FORMAT {
            let expected = !matches!(format, PixelFormat::Yuyv | PixelFormat::Uyvy);
            assert_eq!(format.is_fully_sampled(), expected, "{format}");
        }
    }

    #[test]
    fn a_full_hd_rgb_frame_is_exactly_three_bytes_per_pixel() {
        assert_eq!(PixelFormat::Rgb8.frame_bytes(1920, 1080), Some(6_220_800));
    }

    #[test]
    fn an_odd_width_yuv422_row_has_no_valid_packing() {
        for format in [PixelFormat::Yuyv, PixelFormat::Uyvy] {
            assert_eq!(format.frame_bytes(641, 480), None, "{format}");
            assert_eq!(format.row_bytes(1), None, "{format}");
            assert_eq!(format.row_bytes(2), Some(4), "{format}");
        }
    }

    #[test]
    fn odd_widths_are_fine_for_every_fully_sampled_format() {
        for format in EVERY_FORMAT.iter().filter(|f| f.is_fully_sampled()) {
            assert_eq!(format.pixels_per_group(), 1, "{format}");
            assert_eq!(
                format.row_bytes(7),
                Some(7 * format.bytes_per_pixel()),
                "{format}"
            );
        }
    }

    #[test]
    fn an_overflowing_frame_size_reports_none_rather_than_wrapping() {
        assert_eq!(PixelFormat::Rgba8.frame_bytes(usize::MAX, 2), None);
        assert_eq!(PixelFormat::Rgba8.frame_bytes(usize::MAX / 2, 1), None);
    }

    #[test]
    fn an_empty_frame_is_zero_bytes_not_none() {
        for format in EVERY_FORMAT {
            assert_eq!(format.frame_bytes(0, 0), Some(0), "{format}");
        }
    }

    #[test]
    fn display_uses_the_lowercase_ros_style_encoding_name() {
        assert_eq!(PixelFormat::Bgr8.to_string(), "bgr8");
        assert_eq!(PixelFormat::Yuyv.to_string(), "yuyv");
        assert_eq!(PixelFormat::Uyvy.to_string(), "uyvy");
    }

    #[test]
    fn parse_round_trips_every_format() {
        for format in EVERY_FORMAT {
            assert_eq!(
                PixelFormat::parse(format.as_str()),
                Some(*format),
                "{format}"
            );
        }
        assert_eq!(PixelFormat::parse("nope"), None);
    }
}
