//! Grayscale morphology: [`erode`] (neighbourhood minimum), [`dilate`]
//! (neighbourhood maximum), and the two compositions built on them,
//! [`open`] and [`close`] — over [`PixelFormat::Mono8`] only (see
//! [`crate::error`]'s module doc: morphology is a single-channel op).
//!
//! Defined as the *grayscale* min/max filter rather than a binary-only
//! set operation: applied to a binary (`0`/`255`) frame it is exactly
//! classic binary morphology, and applied to an arbitrary greyscale frame
//! it is still well-defined and useful (a grayscale "erode" darkens narrow
//! bright features, "dilate" thickens them) — the more general definition
//! for the same amount of code.
//!
//! # Structuring element
//!
//! [`StructuringElement::Square`] is a full `(2r + 1)`x`(2r + 1)` block
//! (every neighbour within Chebyshev distance `r`); [`StructuringElement::Cross`]
//! is only the four axis-aligned arms out to Manhattan distance `r` (no
//! diagonals). Both always include the centre pixel itself, so `r = 0` is
//! the identity element for every one of the four operations below.
//!
//! # Border handling: clamp-to-edge
//!
//! The same convention [`crate::sobel`] and [`crate::blur`] use (see
//! [`crate::sobel`]'s module doc for the rationale): a neighbourhood that
//! reaches past the frame reads the nearest edge pixel instead of an
//! assumed background value, so [`erode`] does not spuriously darken a
//! frame's border relative to its interior.

use crate::buffer::ImageBuffer;
use crate::error::Result;
use crate::pixel::PixelFormat;

/// The shape a morphological neighbourhood samples — see this module's doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuringElement {
    /// A full `(2r + 1)`x`(2r + 1)` block.
    Square(u32),
    /// Only the axis-aligned arms out to Manhattan distance `r`.
    Cross(u32),
}

impl StructuringElement {
    /// This element's radius `r`.
    #[must_use]
    pub const fn radius(self) -> u32 {
        match self {
            Self::Square(r) | Self::Cross(r) => r,
        }
    }

    /// Whether offset `(dx, dy)` — already known to satisfy
    /// `max(|dx|, |dy|) <= radius` by construction of the caller's loop
    /// bounds — is actually part of this element's shape.
    const fn contains(self, dx: i64, dy: i64) -> bool {
        match self {
            Self::Square(_) => true,
            Self::Cross(_) => dx == 0 || dy == 0,
        }
    }
}

/// A radius-`1` square — the smallest neighbourhood wider than "just the
/// pixel itself" (`MorphologyOperator`'s own default, see its doc table).
impl Default for StructuringElement {
    fn default() -> Self {
        Self::Square(1)
    }
}

/// Clamps a possibly out-of-range signed coordinate into `0..len` — the
/// same edge-replication [`crate::sobel`] and [`crate::blur`] use.
fn clamp_axis(value: i64, len: usize) -> usize {
    value.clamp(0, len as i64 - 1) as usize
}

/// The shared body of [`erode`] and [`dilate`]: for every pixel, folds
/// `combine` over every sample in its structuring-element neighbourhood
/// (starting from the pixel's own value, so an empty-shaped neighbourhood
/// is unreachable in practice — `(0, 0)` always satisfies
/// [`StructuringElement::contains`] for both variants).
fn morph_filter(
    image: &ImageBuffer,
    element: StructuringElement,
    op: &'static str,
    combine: fn(u8, u8) -> u8,
) -> Result<ImageBuffer> {
    image.require_format(PixelFormat::Mono8, op)?;
    let width = image.width() as usize;
    let height = image.height() as usize;
    if width == 0 || height == 0 {
        return Ok(image.clone());
    }
    let radius = i64::from(element.radius());
    let data = image.data();
    let mut out = vec![0u8; width * height];

    for y in 0..height {
        for x in 0..width {
            let mut acc = data[y * width + x];
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    if !element.contains(dx, dy) {
                        continue;
                    }
                    let sx = clamp_axis(x as i64 + dx, width);
                    let sy = clamp_axis(y as i64 + dy, height);
                    acc = combine(acc, data[sy * width + sx]);
                }
            }
            out[y * width + x] = acc;
        }
    }
    ImageBuffer::new(PixelFormat::Mono8, image.width(), image.height(), out)
}

/// Replaces every pixel with the minimum sample in its `element`
/// neighbourhood — shrinks bright regions, grows dark ones.
///
/// # Errors
///
/// [`crate::VisionError::UnsupportedFormat`] when `image.format()` is not
/// [`PixelFormat::Mono8`].
pub fn erode(image: &ImageBuffer, element: StructuringElement) -> Result<ImageBuffer> {
    morph_filter(image, element, "morphology::erode", u8::min)
}

/// Replaces every pixel with the maximum sample in its `element`
/// neighbourhood — grows bright regions, shrinks dark ones.
///
/// # Errors
///
/// As [`erode`].
pub fn dilate(image: &ImageBuffer, element: StructuringElement) -> Result<ImageBuffer> {
    morph_filter(image, element, "morphology::dilate", u8::max)
}

/// Erosion followed by dilation — removes bright features smaller than
/// `element` (a speck of noise) without changing the size of larger ones
/// much.
///
/// # Errors
///
/// As [`erode`].
pub fn open(image: &ImageBuffer, element: StructuringElement) -> Result<ImageBuffer> {
    dilate(&erode(image, element)?, element)
}

/// Dilation followed by erosion — fills dark gaps smaller than `element`
/// (a pinhole in an otherwise bright region) without changing the size of
/// larger features much.
///
/// # Errors
///
/// As [`erode`].
pub fn close(image: &ImageBuffer, element: StructuringElement) -> Result<ImageBuffer> {
    erode(&dilate(image, element)?, element)
}

/// Which of the four operations [`MorphologyOperator`] applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MorphOp {
    /// [`erode`].
    Erode,
    /// [`dilate`] — the default.
    #[default]
    Dilate,
    /// [`open`].
    Open,
    /// [`close`].
    Close,
}

/// An [`astrs_operator_api::Operator`] applying [`erode`], [`dilate`],
/// [`open`] or [`close`] to every `image` input, publishing the result on
/// `morphed`.
///
/// # Configuration
///
/// | Key | Type | Default | Meaning |
/// |---|---|---|---|
/// | `op` | string | `"dilate"` | `"erode"`, `"dilate"`, `"open"` or `"close"` |
/// | `shape` | string | `"square"` | `"square"` or `"cross"` |
/// | `radius` | integer | `1` | The structuring element's radius, `>= 0` |
#[derive(Debug, Default)]
pub struct MorphologyOperator {
    op: MorphOp,
    element: StructuringElement,
}

impl astrs_operator_api::Operator for MorphologyOperator {
    fn configure(
        &mut self,
        config: &std::collections::BTreeMap<String, astrs_wire::Parameter>,
    ) -> astrs_operator_api::OpResult<()> {
        self.op = match config.get("op").and_then(astrs_wire::Parameter::as_str) {
            Some("erode") => MorphOp::Erode,
            Some("dilate") | None => MorphOp::Dilate,
            Some("open") => MorphOp::Open,
            Some("close") => MorphOp::Close,
            Some(other) => {
                return Err(astrs_operator_api::OpError::failed(format!(
                    "morphology op must be \"erode\", \"dilate\", \"open\" or \"close\", got {other:?}"
                )));
            }
        };
        let radius = match config
            .get("radius")
            .and_then(astrs_wire::Parameter::as_integer)
        {
            Some(value) => u32::try_from(value).map_err(|_| {
                astrs_operator_api::OpError::failed(format!(
                    "morphology radius must fit in 0..=u32::MAX, got {value}"
                ))
            })?,
            None => 1,
        };
        self.element = match config.get("shape").and_then(astrs_wire::Parameter::as_str) {
            Some("square") | None => StructuringElement::Square(radius),
            Some("cross") => StructuringElement::Cross(radius),
            Some(other) => {
                return Err(astrs_operator_api::OpError::failed(format!(
                    "morphology shape must be \"square\" or \"cross\", got {other:?}"
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
        let element = self.element;
        crate::ops::forward_image(event, out, "morphed", |image| match self.op {
            MorphOp::Erode => erode(image, element),
            MorphOp::Dilate => dilate(image, element),
            MorphOp::Open => open(image, element),
            MorphOp::Close => close(image, element),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::error::VisionError;

    fn mono8(data: &[u8], width: u32, height: u32) -> ImageBuffer {
        ImageBuffer::new(PixelFormat::Mono8, width, height, data.to_vec()).unwrap()
    }

    /// A 3x3 solid block of `255` inside a `0` 5x5 frame, exactly one pixel
    /// of margin on every side.
    fn block_5x5() -> ImageBuffer {
        #[rustfmt::skip]
        let data = [
            0, 0, 0, 0, 0,
            0, 255, 255, 255, 0,
            0, 255, 255, 255, 0,
            0, 255, 255, 255, 0,
            0, 0, 0, 0, 0,
        ];
        mono8(&data, 5, 5)
    }

    #[test]
    fn eroding_a_3x3_block_with_a_square_leaves_only_its_centre() {
        // Every pixel of the block has at least one `0` neighbour within
        // Chebyshev distance 1 *except* the exact centre (2, 2), whose
        // full 3x3 neighbourhood is entirely inside the block.
        let eroded = erode(&block_5x5(), StructuringElement::Square(1)).unwrap();
        let mut expected = [0u8; 25];
        expected[2 * 5 + 2] = 255;
        assert_eq!(eroded.data(), &expected);
    }

    #[test]
    fn dilating_a_single_pixel_with_a_square_grows_it_into_a_3x3_block() {
        let mut data = [0u8; 49]; // 7x7, comfortably clear of the border
        data[3 * 7 + 3] = 255;
        let image = mono8(&data, 7, 7);
        let dilated = dilate(&image, StructuringElement::Square(1)).unwrap();
        for y in 2..=4u32 {
            for x in 2..=4u32 {
                assert_eq!(dilated.pixel(x, y), Some(&[255u8][..]), "({x},{y})");
            }
        }
        assert_eq!(
            dilated.pixel(1, 1),
            Some(&[0u8][..]),
            "outside the 3x3 block"
        );
        assert_eq!(
            dilated.pixel(3, 1),
            Some(&[0u8][..]),
            "one above the centre, outside radius 1"
        );
    }

    #[test]
    fn dilating_a_single_pixel_with_a_cross_excludes_the_diagonals() {
        let mut data = [0u8; 25]; // 5x5
        data[2 * 5 + 2] = 255;
        let image = mono8(&data, 5, 5);
        let dilated = dilate(&image, StructuringElement::Cross(1)).unwrap();
        for (x, y) in [(2, 2), (1, 2), (3, 2), (2, 1), (2, 3)] {
            assert_eq!(dilated.pixel(x, y), Some(&[255u8][..]), "arm ({x},{y})");
        }
        for (x, y) in [(1, 1), (3, 1), (1, 3), (3, 3)] {
            assert_eq!(
                dilated.pixel(x, y),
                Some(&[0u8][..]),
                "diagonal ({x},{y}) excluded"
            );
        }
    }

    #[test]
    fn opening_removes_an_isolated_speck() {
        let mut data = [0u8; 25];
        data[2 * 5 + 2] = 255; // one isolated foreground pixel, nothing else
        let image = mono8(&data, 5, 5);
        let opened = open(&image, StructuringElement::Square(1)).unwrap();
        assert!(
            opened.data().iter().all(|&v| v == 0),
            "the speck should vanish entirely"
        );
    }

    #[test]
    fn closing_fills_an_isolated_hole() {
        let mut data = [255u8; 25]; // start all-foreground...
        data[2 * 5 + 2] = 0; // ...except one isolated background pixel
        let image = mono8(&data, 5, 5);
        let closed = close(&image, StructuringElement::Square(1)).unwrap();
        assert!(
            closed.data().iter().all(|&v| v == 255),
            "the hole should be filled"
        );
    }

    #[test]
    fn radius_zero_is_the_identity_for_every_operation() {
        let image = block_5x5();
        for element in [StructuringElement::Square(0), StructuringElement::Cross(0)] {
            assert_eq!(erode(&image, element).unwrap(), image, "{element:?} erode");
            assert_eq!(
                dilate(&image, element).unwrap(),
                image,
                "{element:?} dilate"
            );
            assert_eq!(open(&image, element).unwrap(), image, "{element:?} open");
            assert_eq!(close(&image, element).unwrap(), image, "{element:?} close");
        }
    }

    #[test]
    fn morphology_refuses_a_non_mono8_frame() {
        let rgb = ImageBuffer::zeroed(PixelFormat::Rgb8, 2, 2).unwrap();
        assert!(matches!(
            erode(&rgb, StructuringElement::Square(1)),
            Err(VisionError::UnsupportedFormat {
                op: "morphology::erode",
                ..
            })
        ));
        assert!(matches!(
            dilate(&rgb, StructuringElement::Square(1)),
            Err(VisionError::UnsupportedFormat {
                op: "morphology::dilate",
                ..
            })
        ));
    }

    #[test]
    fn morphology_on_an_empty_frame_is_the_identity() {
        let empty = ImageBuffer::zeroed(PixelFormat::Mono8, 0, 0).unwrap();
        assert_eq!(erode(&empty, StructuringElement::Square(1)).unwrap(), empty);
        assert_eq!(
            dilate(&empty, StructuringElement::Square(1)).unwrap(),
            empty
        );
    }
}
