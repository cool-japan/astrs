//! Drawing primitives: [`draw_line`] (Bresenham), [`draw_rect`] and
//! [`draw_circle`] (midpoint circle), over any fully-sampled [`crate::PixelFormat`]
//! (see [`crate::error`]'s module doc — drawing is a geometric op).
//!
//! **Text is explicitly out of scope** — this module draws shapes, not
//! glyphs; a caller wanting to label a bounding box needs a font-rendering
//! crate this workspace does not carry (blueprint §18.1's Pure Rust policy
//! makes that a real cost, not a shrug), and every one of the geometric
//! primitives here still composes with [`crate::camera`]'s undistort maps
//! and [`crate::components`]'s bounding boxes without one.
//!
//! # Off-canvas coordinates clip silently
//!
//! Every primitive takes signed (`i64`) coordinates and simply skips
//! whichever of its pixels fall outside `0..width`/`0..height` — the same
//! "clip, don't fail" convention [`crate::buffer::ImageBuffer::put_pixel`]
//! already documents for a single out-of-range pixel. A shape entirely off
//! canvas draws nothing at all, successfully.
//!
//! # `color`'s length must match the image's channel count
//!
//! One byte per channel, in the image's own channel order (so `[b, g, r]`
//! for a [`crate::PixelFormat::Bgr8`] target) — checked once per call, not once
//! per pixel.

use crate::buffer::ImageBuffer;
use crate::error::{Result, VisionError};

/// [`VisionError::ChannelCountMismatch`] unless `color.len()` matches
/// `image`'s channel count.
fn check_color(image: &ImageBuffer, color: &[u8]) -> Result<()> {
    let expected = image.bytes_per_pixel();
    if color.len() != expected {
        return Err(VisionError::ChannelCountMismatch {
            format: image.format(),
            expected,
            actual: color.len(),
        });
    }
    Ok(())
}

/// Writes `color` at `(x, y)`, silently doing nothing when either
/// coordinate is negative or past the frame — see this module's doc.
///
/// `color.len()` is assumed already checked by the caller (every public
/// function in this module does so once, up front); this is the
/// unchecked, allocation-free primitive the hot loops below share.
fn set_pixel_clipped(image: &mut ImageBuffer, x: i64, y: i64, color: &[u8]) {
    let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y)) else {
        return;
    };
    if x >= image.width() || y >= image.height() {
        return;
    }
    let bpp = color.len();
    let start = (x as usize) * bpp;
    if let Some(row) = image.row_mut(y) {
        row[start..start + bpp].copy_from_slice(color);
    }
}

/// Draws a straight line from `from` to `to` (inclusive of both endpoints)
/// with Bresenham's algorithm.
///
/// # Errors
///
/// [`VisionError::UnsupportedFormat`] for a packed 4:2:2 frame;
/// [`VisionError::ChannelCountMismatch`] when `color.len()` does not match
/// `image`'s channel count.
pub fn draw_line(
    image: &mut ImageBuffer,
    from: (i64, i64),
    to: (i64, i64),
    color: &[u8],
) -> Result<()> {
    image.require_fully_sampled("draw::draw_line")?;
    check_color(image, color)?;

    let (mut x, mut y) = from;
    let (x1, y1) = to;
    let dx = (x1 - x).abs();
    let dy = -(y1 - y).abs();
    let step_x = if x < x1 { 1 } else { -1 };
    let step_y = if y < y1 { 1 } else { -1 };
    let mut err = dx + dy;

    loop {
        set_pixel_clipped(image, x, y, color);
        if x == x1 && y == y1 {
            break;
        }
        let doubled = 2 * err;
        if doubled >= dy {
            err += dy;
            x += step_x;
        }
        if doubled <= dx {
            err += dx;
            y += step_y;
        }
    }
    Ok(())
}

/// Draws an axis-aligned rectangle spanning `top_left` to `bottom_right`
/// (both inclusive, in either order — the two corners are normalised
/// internally). `filled` selects a solid rectangle over a one-pixel-wide
/// outline.
///
/// # Errors
///
/// As [`draw_line`].
pub fn draw_rect(
    image: &mut ImageBuffer,
    top_left: (i64, i64),
    bottom_right: (i64, i64),
    color: &[u8],
    filled: bool,
) -> Result<()> {
    image.require_fully_sampled("draw::draw_rect")?;
    check_color(image, color)?;

    let min_x = top_left.0.min(bottom_right.0);
    let max_x = top_left.0.max(bottom_right.0);
    let min_y = top_left.1.min(bottom_right.1);
    let max_y = top_left.1.max(bottom_right.1);

    if filled {
        for y in min_y..=max_y {
            for x in min_x..=max_x {
                set_pixel_clipped(image, x, y, color);
            }
        }
    } else {
        for x in min_x..=max_x {
            set_pixel_clipped(image, x, min_y, color);
            set_pixel_clipped(image, x, max_y, color);
        }
        for y in min_y..=max_y {
            set_pixel_clipped(image, min_x, y, color);
            set_pixel_clipped(image, max_x, y, color);
        }
    }
    Ok(())
}

/// The eight points a midpoint-circle octant sample `(x, y)` (measured from
/// the centre) mirrors to, in the algorithm's own working coordinates —
/// [`draw_circle`]'s outline path offsets each by `center` before plotting.
fn circle_octant(x: i64, y: i64) -> [(i64, i64); 8] {
    [
        (x, y),
        (y, x),
        (-y, x),
        (-x, y),
        (-x, -y),
        (-y, -x),
        (y, -x),
        (x, -y),
    ]
}

/// Draws a circle of `radius` centred on `center`. `filled` selects a solid
/// disc (every pixel within `radius` of the centre, by Euclidean distance)
/// over a one-pixel-wide outline (the midpoint circle algorithm — the same
/// family as [`draw_line`]'s Bresenham, generalised to a circular arc).
///
/// The filled path is a straightforward `O(radius^2)` bounding-box scan
/// rather than a row-span variant of the same asymptotic saving the
/// outline path gets: every caller in this crate draws marker- and
/// keypoint-sized circles (single-digit-to-low-double-digit pixel radii),
/// where the simpler code has no measurable cost.
///
/// # Errors
///
/// As [`draw_line`].
pub fn draw_circle(
    image: &mut ImageBuffer,
    center: (i64, i64),
    radius: u32,
    color: &[u8],
    filled: bool,
) -> Result<()> {
    image.require_fully_sampled("draw::draw_circle")?;
    check_color(image, color)?;
    let radius = i64::from(radius);

    if filled {
        let radius_sq = radius * radius;
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                if dx * dx + dy * dy <= radius_sq {
                    set_pixel_clipped(image, center.0 + dx, center.1 + dy, color);
                }
            }
        }
        return Ok(());
    }

    let mut x = radius;
    let mut y = 0i64;
    let mut decision = 1 - radius;
    while x >= y {
        for (ox, oy) in circle_octant(x, y) {
            set_pixel_clipped(image, center.0 + ox, center.1 + oy, color);
        }
        y += 1;
        if decision < 0 {
            decision += 2 * y + 1;
        } else {
            x -= 1;
            decision += 2 * (y - x) + 1;
        }
    }
    Ok(())
}

/// An [`astrs_operator_api::Operator`] drawing one configured rectangle
/// outline onto every `image` input, publishing the annotated frame on
/// `annotated` — the shape this module's primitives reduce to when the
/// shape itself is fixed by configuration rather than computed per frame
/// (a calibration ROI overlay, a fixed crop guide).
///
/// # Configuration
///
/// | Key | Type | Default | Meaning |
/// |---|---|---|---|
/// | `x`, `y` | integer | `0` | The rectangle's first corner |
/// | `width`, `height` | integer | `0` | Extent from `(x, y)` |
/// | `color` | integer list | all-`255` | One value per channel, `0..=255` |
#[derive(Debug, Default)]
pub struct DrawRectOperator {
    top_left: (i64, i64),
    bottom_right: (i64, i64),
    color: Vec<u8>,
}

impl astrs_operator_api::Operator for DrawRectOperator {
    fn configure(
        &mut self,
        config: &std::collections::BTreeMap<String, astrs_wire::Parameter>,
    ) -> astrs_operator_api::OpResult<()> {
        let get_i64 = |key: &str| {
            config
                .get(key)
                .and_then(astrs_wire::Parameter::as_integer)
                .unwrap_or(0)
        };
        let x = get_i64("x");
        let y = get_i64("y");
        let width = get_i64("width");
        let height = get_i64("height");
        self.top_left = (x, y);
        self.bottom_right = (x + width, y + height);

        self.color = match config
            .get("color")
            .and_then(astrs_wire::Parameter::as_list_int)
        {
            Some(values) => values
                .iter()
                .map(|&v| {
                    u8::try_from(v).map_err(|_| {
                        astrs_operator_api::OpError::failed(format!(
                            "draw color channel must fit in 0..=255, got {v}"
                        ))
                    })
                })
                .collect::<astrs_operator_api::OpResult<Vec<u8>>>()?,
            None => Vec::new(),
        };
        Ok(())
    }

    fn on_event(
        &mut self,
        event: &astrs_operator_api::OpEvent,
        out: &mut astrs_operator_api::OpOutput,
    ) -> astrs_operator_api::OpResult<astrs_operator_api::Status> {
        crate::ops::forward_image(event, out, "annotated", |image| {
            let mut annotated = image.clone();
            let color = if self.color.is_empty() {
                vec![255u8; annotated.bytes_per_pixel()]
            } else {
                self.color.clone()
            };
            draw_rect(
                &mut annotated,
                self.top_left,
                self.bottom_right,
                &color,
                false,
            )?;
            Ok(annotated)
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::pixel::PixelFormat;

    fn canvas(width: u32, height: u32) -> ImageBuffer {
        ImageBuffer::zeroed(PixelFormat::Mono8, width, height).unwrap()
    }

    fn lit(image: &ImageBuffer) -> Vec<(u32, u32)> {
        let mut points = Vec::new();
        for y in 0..image.height() {
            for x in 0..image.width() {
                if image.pixel(x, y) == Some(&[255u8][..]) {
                    points.push((x, y));
                }
            }
        }
        points
    }

    #[test]
    fn a_horizontal_line_lights_every_column_on_one_row() {
        let mut image = canvas(5, 3);
        draw_line(&mut image, (0, 1), (3, 1), &[255]).unwrap();
        assert_eq!(lit(&image), vec![(0, 1), (1, 1), (2, 1), (3, 1)]);
    }

    #[test]
    fn a_45_degree_line_is_the_exact_diagonal() {
        let mut image = canvas(4, 4);
        draw_line(&mut image, (0, 0), (3, 3), &[255]).unwrap();
        assert_eq!(lit(&image), vec![(0, 0), (1, 1), (2, 2), (3, 3)]);
    }

    #[test]
    fn a_shallow_line_matches_the_hand_traced_bresenham_sequence() {
        // (0,0) -> (4,2): hand-traced against this module's own
        // implementation in this task's derivation; independently, the
        // step pattern (y advances roughly every other x, for a slope of
        // exactly 1/2) is the textbook expectation for this algorithm.
        let mut image = canvas(5, 3);
        draw_line(&mut image, (0, 0), (4, 2), &[255]).unwrap();
        assert_eq!(lit(&image), vec![(0, 0), (1, 1), (2, 1), (3, 2), (4, 2)]);
    }

    #[test]
    fn a_line_partly_off_canvas_clips_silently() {
        let mut image = canvas(3, 3);
        draw_line(&mut image, (-2, 1), (5, 1), &[255]).unwrap();
        assert_eq!(lit(&image), vec![(0, 1), (1, 1), (2, 1)]);
    }

    #[test]
    fn rect_outline_lights_exactly_the_perimeter() {
        let mut image = canvas(5, 5);
        draw_rect(&mut image, (1, 1), (3, 3), &[255], false).unwrap();
        assert_eq!(
            lit(&image),
            vec![
                (1, 1),
                (2, 1),
                (3, 1),
                (1, 2),
                (3, 2),
                (1, 3),
                (2, 3),
                (3, 3)
            ],
        );
    }

    #[test]
    fn rect_filled_lights_the_whole_block() {
        let mut image = canvas(5, 5);
        draw_rect(&mut image, (1, 1), (3, 3), &[255], true).unwrap();
        assert_eq!(lit(&image).len(), 9);
        assert_eq!(image.pixel(2, 2), Some(&[255u8][..]));
    }

    #[test]
    fn rect_corners_are_order_independent() {
        let mut forward = canvas(5, 5);
        draw_rect(&mut forward, (1, 1), (3, 3), &[255], false).unwrap();
        let mut backward = canvas(5, 5);
        draw_rect(&mut backward, (3, 3), (1, 1), &[255], false).unwrap();
        assert_eq!(forward, backward);
    }

    #[test]
    fn circle_outline_radius_one_is_a_four_neighbour_diamond() {
        let mut image = canvas(5, 5);
        draw_circle(&mut image, (2, 2), 1, &[255], false).unwrap();
        let mut points = lit(&image);
        points.sort_unstable();
        assert_eq!(points, vec![(1, 2), (2, 1), (2, 3), (3, 2)]);
    }

    #[test]
    fn circle_filled_radius_one_adds_the_centre() {
        let mut image = canvas(5, 5);
        draw_circle(&mut image, (2, 2), 1, &[255], true).unwrap();
        let mut points = lit(&image);
        points.sort_unstable();
        assert_eq!(points, vec![(1, 2), (2, 1), (2, 2), (2, 3), (3, 2)]);
    }

    #[test]
    fn draw_refuses_a_packed_yuyv_frame() {
        let mut yuyv = ImageBuffer::zeroed(PixelFormat::Yuyv, 4, 1).unwrap();
        assert!(matches!(
            draw_line(&mut yuyv, (0, 0), (1, 0), &[0, 0]),
            Err(VisionError::UnsupportedFormat {
                op: "draw::draw_line",
                ..
            })
        ));
    }

    #[test]
    fn draw_rejects_a_color_with_the_wrong_channel_count() {
        let mut image = canvas(3, 3);
        assert!(matches!(
            draw_line(&mut image, (0, 0), (1, 0), &[1, 2, 3]),
            Err(VisionError::ChannelCountMismatch {
                expected: 1,
                actual: 3,
                ..
            })
        ));
    }
}
