//! Connected-component labeling over [`PixelFormat::Mono8`] (see
//! [`crate::error`]'s module doc — a single-channel op, like
//! [`crate::threshold`] and [`crate::morphology`] it typically follows in a
//! pipeline). Every sample not exactly `0` is foreground — the natural
//! reading of a [`crate::threshold`] or [`crate::morphology`] result, and
//! of an arbitrary [`PixelFormat::Mono8`] frame besides.
//!
//! [`label_components`] does both halves of the job in one pass: the
//! [`Labels`] plane (one `u16` label per pixel, `0` reserved for
//! background — see [`crate::VisionError::TooManyComponents`]'s own doc for
//! why that caps this module at `65535` distinct components), and a
//! [`ComponentStats`] per label (area, bounding box, centroid), computed
//! incrementally as each component's pixels are visited rather than in a
//! second pass over [`Labels`].
//!
//! # Algorithm: iterative flood fill, not two-pass union-find
//!
//! Each unlabeled foreground pixel, scanned in row-major order, starts a
//! breadth-first flood fill (an explicit [`std::collections::VecDeque`],
//! not recursion — a recursive flood fill's stack depth is bounded by the
//! component's *pixel count*, which a large solid region blows past long
//! before any reasonable process stack limit) that labels its entire
//! component before the outer scan moves on. This is the same `O(pixels)`
//! total work as two-pass union-find, without union-find's own bookkeeping
//! — a deliberate simplicity-over-cleverness trade for a labeling pass
//! that is not, in this crate's own pipeline, the bottleneck
//! [`crate::blur`] or [`crate::resize`] are.

use std::collections::VecDeque;

use crate::buffer::ImageBuffer;
use crate::error::{Result, VisionError};
use crate::pixel::PixelFormat;

/// Which neighbours [`label_components`] treats as connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Connectivity {
    /// Only the four axis-aligned neighbours — the default. Two
    /// foreground pixels touching only at a corner are *not* connected.
    #[default]
    Four,
    /// All eight neighbours, corners included.
    Eight,
}

impl Connectivity {
    /// This connectivity's neighbour offsets.
    const fn offsets(self) -> &'static [(i32, i32)] {
        match self {
            Self::Four => &[(0, -1), (0, 1), (-1, 0), (1, 0)],
            Self::Eight => &[
                (-1, -1),
                (0, -1),
                (1, -1),
                (-1, 0),
                (1, 0),
                (-1, 1),
                (0, 1),
                (1, 1),
            ],
        }
    }
}

/// A `width`x`height` plane of `u16` component labels, `0` meaning
/// background — the same shape [`astrs_node_api::message::Mask`] carries on
/// the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Labels {
    width: u32,
    height: u32,
    data: Vec<u16>,
}

impl Labels {
    /// The plane's width.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The plane's height.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The raw label plane, row-major.
    #[must_use]
    pub fn data(&self) -> &[u16] {
        &self.data
    }

    /// The label at `(x, y)` (`0` for background), or [`None`] outside the
    /// plane.
    #[must_use]
    pub fn label_at(&self, x: u32, y: u32) -> Option<u16> {
        if x >= self.width || y >= self.height {
            return None;
        }
        self.data
            .get((y as usize) * (self.width as usize) + (x as usize))
            .copied()
    }

    /// Encodes this plane as a `std/vision/v1/Mask` message.
    ///
    /// # Errors
    ///
    /// Whatever [`astrs_node_api::message::Mask::new`] reports (in
    /// practice unreachable — this plane's own dimensions always agree
    /// with its own data length).
    pub fn to_mask(&self) -> Result<astrs_node_api::message::Mask> {
        Ok(astrs_node_api::message::Mask::new(
            self.width,
            self.height,
            self.data.clone(),
        )?)
    }
}

/// One labeled component's summary statistics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ComponentStats {
    /// This component's label (`1..=65535`; `0` is never a component's own
    /// label, only the background's).
    pub label: u16,
    /// Pixel count.
    pub area: u32,
    /// The bounding box's smallest coordinates (inclusive).
    pub min_x: u32,
    /// The bounding box's smallest coordinates (inclusive).
    pub min_y: u32,
    /// The bounding box's largest coordinates (inclusive).
    pub max_x: u32,
    /// The bounding box's largest coordinates (inclusive).
    pub max_y: u32,
    /// The mean `x` coordinate of every pixel in the component.
    pub centroid_x: f64,
    /// The mean `y` coordinate of every pixel in the component.
    pub centroid_y: f64,
}

impl ComponentStats {
    /// The bounding box's width in pixels (`max_x - min_x + 1`).
    #[must_use]
    pub const fn bbox_width(&self) -> u32 {
        self.max_x - self.min_x + 1
    }

    /// The bounding box's height in pixels (`max_y - min_y + 1`).
    #[must_use]
    pub const fn bbox_height(&self) -> u32 {
        self.max_y - self.min_y + 1
    }
}

/// Labels every connected foreground (`!= 0`) region of `image`, returning
/// the label plane and one [`ComponentStats`] per label, in label order.
///
/// # Errors
///
/// [`VisionError::UnsupportedFormat`] when `image.format()` is not
/// [`PixelFormat::Mono8`]; [`VisionError::TooManyComponents`] when the
/// frame has more than `65535` distinct components (see this module's
/// doc).
pub fn label_components(
    image: &ImageBuffer,
    connectivity: Connectivity,
) -> Result<(Labels, Vec<ComponentStats>)> {
    image.require_format(PixelFormat::Mono8, "components::label_components")?;
    let width = image.width() as usize;
    let height = image.height() as usize;
    let source = image.data();
    let mut labels = vec![0u16; width * height];
    let mut stats = Vec::new();
    let mut next_label: u32 = 1;
    let mut queue: VecDeque<(usize, usize)> = VecDeque::new();

    for start_y in 0..height {
        for start_x in 0..width {
            let start_index = start_y * width + start_x;
            if source[start_index] == 0 || labels[start_index] != 0 {
                continue;
            }
            if next_label > u32::from(u16::MAX) {
                return Err(VisionError::TooManyComponents {
                    found: next_label as usize,
                    max: u16::MAX as usize,
                });
            }
            let label = next_label as u16;
            next_label += 1;

            let mut area = 0u32;
            let (mut min_x, mut min_y) = (start_x as u32, start_y as u32);
            let (mut max_x, mut max_y) = (start_x as u32, start_y as u32);
            let mut sum_x = 0.0f64;
            let mut sum_y = 0.0f64;

            queue.clear();
            queue.push_back((start_x, start_y));
            labels[start_index] = label;

            while let Some((x, y)) = queue.pop_front() {
                area += 1;
                min_x = min_x.min(x as u32);
                max_x = max_x.max(x as u32);
                min_y = min_y.min(y as u32);
                max_y = max_y.max(y as u32);
                sum_x += x as f64;
                sum_y += y as f64;

                for &(dx, dy) in connectivity.offsets() {
                    let nx = x as i64 + i64::from(dx);
                    let ny = y as i64 + i64::from(dy);
                    if nx < 0 || ny < 0 || nx as usize >= width || ny as usize >= height {
                        continue;
                    }
                    let (nx, ny) = (nx as usize, ny as usize);
                    let neighbor_index = ny * width + nx;
                    if source[neighbor_index] != 0 && labels[neighbor_index] == 0 {
                        labels[neighbor_index] = label;
                        queue.push_back((nx, ny));
                    }
                }
            }

            stats.push(ComponentStats {
                label,
                area,
                min_x,
                min_y,
                max_x,
                max_y,
                centroid_x: sum_x / f64::from(area),
                centroid_y: sum_y / f64::from(area),
            });
        }
    }

    Ok((
        Labels {
            width: image.width(),
            height: image.height(),
            data: labels,
        },
        stats,
    ))
}

/// An [`astrs_operator_api::Operator`] applying [`label_components`] to
/// every `image` input, publishing the label plane on `labels` as a
/// `std/vision/v1/Mask`.
///
/// Per-component [`ComponentStats`] are not published — a `Mask` has no
/// slot for them, and forcing them into an unrelated message shape (a
/// `Detections`' `scores` field means detector confidence, not component
/// area) would be a worse fit than simply leaving stats to a caller using
/// [`label_components`] directly, which this operator itself does
/// internally.
///
/// # Configuration
///
/// | Key | Type | Default | Meaning |
/// |---|---|---|---|
/// | `connectivity` | integer | `4` | `4` or `8` |
#[derive(Debug, Default)]
pub struct ComponentsOperator {
    connectivity: Connectivity,
}

impl astrs_operator_api::Operator for ComponentsOperator {
    fn configure(
        &mut self,
        config: &std::collections::BTreeMap<String, astrs_wire::Parameter>,
    ) -> astrs_operator_api::OpResult<()> {
        self.connectivity = match config
            .get("connectivity")
            .and_then(astrs_wire::Parameter::as_integer)
        {
            Some(4) | None => Connectivity::Four,
            Some(8) => Connectivity::Eight,
            Some(other) => {
                return Err(astrs_operator_api::OpError::failed(format!(
                    "components connectivity must be 4 or 8, got {other}"
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
                let (labels, _stats) = label_components(&image, self.connectivity)?;
                let mask = labels.to_mask()?;
                out.send("labels", metadata.clone(), &mask)?;
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

    fn mono8(data: &[u8], width: u32, height: u32) -> ImageBuffer {
        ImageBuffer::new(PixelFormat::Mono8, width, height, data.to_vec()).unwrap()
    }

    #[test]
    fn two_separated_blobs_get_two_labels_with_correct_stats() {
        #[rustfmt::skip]
        let data = [
            255, 255, 0, 0, 0,
            255, 255, 0, 0, 0,
            0, 0, 0, 255, 255,
            0, 0, 0, 255, 255,
            0, 0, 0, 0, 0,
        ];
        let image = mono8(&data, 5, 5);
        let (labels, stats) = label_components(&image, Connectivity::Four).unwrap();
        assert_eq!(stats.len(), 2);

        assert_eq!(labels.label_at(0, 0), Some(1));
        assert_eq!(labels.label_at(1, 1), Some(1));
        assert_eq!(labels.label_at(3, 2), Some(2));
        assert_eq!(labels.label_at(4, 3), Some(2));
        assert_eq!(labels.label_at(2, 2), Some(0), "background stays label 0");

        assert_eq!(stats[0].label, 1);
        assert_eq!(stats[0].area, 4);
        assert_eq!(
            (
                stats[0].min_x,
                stats[0].min_y,
                stats[0].max_x,
                stats[0].max_y
            ),
            (0, 0, 1, 1)
        );
        assert_eq!((stats[0].centroid_x, stats[0].centroid_y), (0.5, 0.5));

        assert_eq!(stats[1].label, 2);
        assert_eq!(stats[1].area, 4);
        assert_eq!(
            (
                stats[1].min_x,
                stats[1].min_y,
                stats[1].max_x,
                stats[1].max_y
            ),
            (3, 2, 4, 3)
        );
        assert_eq!((stats[1].centroid_x, stats[1].centroid_y), (3.5, 2.5));
        assert_eq!(stats[1].bbox_width(), 2);
        assert_eq!(stats[1].bbox_height(), 2);
    }

    #[test]
    fn diagonal_touch_is_two_components_under_four_connectivity() {
        let image = mono8(&[255, 0, 0, 255], 2, 2); // (0,0) and (1,1)
        let (_, stats) = label_components(&image, Connectivity::Four).unwrap();
        assert_eq!(stats.len(), 2);
    }

    #[test]
    fn diagonal_touch_is_one_component_under_eight_connectivity() {
        let image = mono8(&[255, 0, 0, 255], 2, 2);
        let (labels, stats) = label_components(&image, Connectivity::Eight).unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(labels.label_at(0, 0), labels.label_at(1, 1));
        assert_eq!(stats[0].area, 2);
        assert_eq!(
            (
                stats[0].min_x,
                stats[0].min_y,
                stats[0].max_x,
                stats[0].max_y
            ),
            (0, 0, 1, 1)
        );
    }

    #[test]
    fn an_all_background_frame_has_no_components() {
        let image = mono8(&[0; 9], 3, 3);
        let (labels, stats) = label_components(&image, Connectivity::Four).unwrap();
        assert!(stats.is_empty());
        assert!(labels.data().iter().all(|&v| v == 0));
    }

    #[test]
    fn labels_convert_to_a_wire_mask() {
        let image = mono8(&[255, 0, 0, 255], 2, 2);
        let (labels, _) = label_components(&image, Connectivity::Eight).unwrap();
        let mask = labels.to_mask().unwrap();
        assert_eq!(mask.label_at(0, 0), Some(1));
        assert_eq!(mask.label_at(1, 1), Some(1));
        assert_eq!(mask.label_at(1, 0), Some(0));
    }

    #[test]
    fn components_refuses_a_non_mono8_frame() {
        let rgb = ImageBuffer::zeroed(PixelFormat::Rgb8, 2, 2).unwrap();
        assert!(matches!(
            label_components(&rgb, Connectivity::Four),
            Err(VisionError::UnsupportedFormat {
                op: "components::label_components",
                ..
            })
        ));
    }

    #[test]
    fn more_than_the_u16_label_budget_is_reported_not_silently_wrapped() {
        // 512x512, foreground only at even (x, y): 256*256 = 65536
        // pixels, each isolated from every other by a gap of 1 in every
        // direction (so no two are neighbours even under eight-way
        // connectivity) -- one more than the 65535-label budget.
        let side = 512usize;
        let mut data = vec![0u8; side * side];
        for y in (0..side).step_by(2) {
            for x in (0..side).step_by(2) {
                data[y * side + x] = 255;
            }
        }
        let image = mono8(&data, side as u32, side as u32);
        let error = label_components(&image, Connectivity::Eight).unwrap_err();
        assert!(matches!(
            error,
            VisionError::TooManyComponents {
                found: 65536,
                max: 65535
            }
        ));
    }

    // ---- Operators ----
    //
    // `ComponentsOperator` does not fit `crate::ops::forward_image` (its
    // output is a `std/vision/v1/Mask`, not an `Image` -- see its own doc)
    // and writes its own `on_event`, so it needs its own end-to-end
    // coverage rather than riding along with the `forward_image` family's
    // (see `threshold.rs`'s own "---- Operators ----" section).

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

    fn decode_mask_payload(payload: &[u8]) -> astrs_node_api::message::Mask {
        use astrs_node_api::message::AstrsMessage;
        let batch = astrs_data::ipc::decode_payload(payload).unwrap();
        astrs_node_api::message::Mask::from_record_batch(&batch).unwrap()
    }

    #[test]
    fn components_operator_publishes_a_mask_matching_the_direct_call() {
        use astrs_operator_api::{OpOutput, Operator, Status};

        #[rustfmt::skip]
        let data = [
            255, 255, 0, 0, 0,
            255, 255, 0, 0, 0,
            0, 0, 0, 255, 255,
            0, 0, 0, 255, 255,
            0, 0, 0, 0, 0,
        ];
        let image = mono8(&data, 5, 5);
        let mut op = ComponentsOperator::default();
        let mut config = std::collections::BTreeMap::new();
        config.insert("connectivity".to_owned(), astrs_wire::Parameter::Integer(8));
        op.configure(&config).unwrap();

        let mut out = OpOutput::new();
        let status = op
            .on_event(&input_event(image_payload(&image)), &mut out)
            .unwrap();
        assert_eq!(status, Status::Continue);

        let sends = out.drain();
        assert_eq!(sends.len(), 1);
        assert_eq!(
            sends[0].id().as_str(),
            "labels",
            "must match this operator's own doc"
        );

        let published = decode_mask_payload(sends[0].payload());
        let (labels, _stats) = label_components(&image, Connectivity::Eight).unwrap();
        let direct = labels.to_mask().unwrap();
        assert_eq!(
            published, direct,
            "the operator must publish exactly the mask `label_components` + `to_mask` produces"
        );
    }

    #[test]
    fn components_operator_finishes_on_stop() {
        use astrs_operator_api::{OpEvent, OpOutput, Operator, Status};

        let mut op = ComponentsOperator::default();
        let mut out = OpOutput::new();
        let stop = OpEvent::Stop {
            cause: astrs_wire::StopCause::Requested,
            grace: None,
        };
        assert_eq!(op.on_event(&stop, &mut out).unwrap(), Status::Finished);
        assert!(out.is_empty());
    }
}
