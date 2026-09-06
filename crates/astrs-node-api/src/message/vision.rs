//! `std/vision/v1` — perception outputs (§24.3).
//!
//! | Type | Layout |
//! |---|---|
//! | [`Detections`] | `{boxes: List<FixedSizeList<Float32, 4>>, scores: List<Float32>, labels: List<UInt32>}` |
//! | [`Keypoints`] | `{points: List<FixedSizeList<Float32, 2>>, scores: List<Float32>, labels: List<UInt32>}` |
//! | [`Mask`] | `{width: UInt32, height: UInt32, data: List<UInt16>}` |
//!
//! The three `List` columns of [`Detections`] and [`Keypoints`] are
//! *index-aligned*: entry *i* of `scores` is the confidence of entry *i* of
//! `boxes`. Nothing in the columnar layout enforces that — three lists of
//! different lengths are a well-formed `Struct` — so both types check it on
//! encode and on decode, which is the difference between "the payload is
//! malformed" and "the labels are silently off by one".
//!
//! # Examples
//!
//! ```
//! use astrs_node_api::message::{AstrsMessage, BoundingBox, Detections};
//!
//! let detections = Detections::new(
//!     vec![BoundingBox::new(0.0, 0.0, 10.0, 20.0)],
//!     vec![0.93],
//!     vec![7],
//! )?;
//! let batch = detections.to_record_batch()?;
//! assert_eq!(Detections::from_record_batch(&batch)?.len(), 1);
//! # Ok::<(), astrs_data::DataError>(())
//! ```

use astrs_data::{ArrayRef, AstrsMessage, DataError, DataType, Field, RecordBatch, Result};

use super::{build, read, single_row_column};

/// One axis-aligned detection box, in image pixel coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, PartialOrd)]
pub struct BoundingBox {
    /// The left edge.
    pub x: f32,
    /// The top edge.
    pub y: f32,
    /// The width.
    pub width: f32,
    /// The height.
    pub height: f32,
}

impl BoundingBox {
    /// A box from its `xywh` components.
    #[must_use]
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The components in `xywh` order — the layout's own order.
    #[must_use]
    pub const fn to_array(self) -> [f32; 4] {
        [self.x, self.y, self.width, self.height]
    }

    /// A box from an `xywh` array.
    #[must_use]
    pub const fn from_array(values: [f32; 4]) -> Self {
        Self::new(values[0], values[1], values[2], values[3])
    }

    /// The box's area, clamped at zero for a degenerate box.
    #[must_use]
    pub fn area(self) -> f32 {
        self.width.max(0.0) * self.height.max(0.0)
    }
}

impl From<[f32; 4]> for BoundingBox {
    fn from(values: [f32; 4]) -> Self {
        Self::from_array(values)
    }
}

/// One landmark, in image pixel coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, PartialOrd)]
pub struct Keypoint {
    /// The horizontal coordinate.
    pub x: f32,
    /// The vertical coordinate.
    pub y: f32,
}

impl Keypoint {
    /// A keypoint from its coordinates.
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// The coordinates in `xy` order.
    #[must_use]
    pub const fn to_array(self) -> [f32; 2] {
        [self.x, self.y]
    }
}

impl From<[f32; 2]> for Keypoint {
    fn from(values: [f32; 2]) -> Self {
        Self::new(values[0], values[1])
    }
}

/// Detection boxes with per-box confidence and label —
/// `std/vision/v1/Detections`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Detections {
    /// The boxes.
    pub boxes: Vec<BoundingBox>,
    /// The per-box confidences, index-aligned with `boxes`.
    pub scores: Vec<f32>,
    /// The per-box class labels, index-aligned with `boxes`.
    pub labels: Vec<u32>,
}

impl Detections {
    /// Detections whose three lists are the same length.
    ///
    /// # Errors
    ///
    /// [`DataError::ColumnLengthMismatch`] when they are not.
    pub fn new(boxes: Vec<BoundingBox>, scores: Vec<f32>, labels: Vec<u32>) -> Result<Self> {
        let value = Self {
            boxes,
            scores,
            labels,
        };
        value.check_alignment()?;
        Ok(value)
    }

    /// An empty detection set.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            boxes: Vec::new(),
            scores: Vec::new(),
            labels: Vec::new(),
        }
    }

    /// How many detections the message carries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.boxes.len()
    }

    /// Whether the message carries no detections.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }

    /// Checks that the three index-aligned lists agree on length.
    ///
    /// # Errors
    ///
    /// [`DataError::ColumnLengthMismatch`] naming the first list that
    /// disagrees.
    pub fn check_alignment(&self) -> Result<()> {
        check_three(
            ("boxes", self.boxes.len()),
            ("scores", self.scores.len()),
            ("labels", self.labels.len()),
        )
    }

    /// The columnar layout of this type.
    #[must_use]
    pub fn layout() -> DataType {
        DataType::strukt([
            Field::required(
                "boxes",
                DataType::list(Field::required(
                    "xywh",
                    DataType::fixed_size_list(Field::required("v", DataType::Float32), 4),
                )),
            ),
            Field::required(
                "scores",
                DataType::list(Field::required("score", DataType::Float32)),
            ),
            Field::required(
                "labels",
                DataType::list(Field::required("label", DataType::UInt32)),
            ),
        ])
    }
}

impl AstrsMessage for Detections {
    const URN: &'static str = "std/vision/v1/Detections";

    fn data_type() -> DataType {
        Self::layout()
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        self.check_alignment()?;
        let flat: Vec<f32> = self
            .boxes
            .iter()
            .flat_map(|value| value.to_array())
            .collect();
        let column = build::structure(vec![
            (
                "boxes",
                build::fixed_tuple_lists::<f32>("xywh", "v", 4, &[&flat])?,
            ),
            (
                "scores",
                build::primitive_lists::<f32>("score", &[&self.scores])?,
            ),
            (
                "labels",
                build::primitive_lists::<u32>("label", &[&self.labels])?,
            ),
        ])?;
        Ok(RecordBatch::from_payload(column))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        let (flat, width) = read::fixed_tuple_list_at::<f32>(read::child(column, "boxes")?, 0)?;
        if width != 4 {
            return Err(DataError::ChildLengthMismatch {
                expected: 4,
                actual: width,
            });
        }
        // The width check above is what makes `as_chunks::<4>` exact: its
        // remainder is empty by construction, and each chunk arrives as the
        // `[f32; 4]` `BoundingBox`'s own `From` takes.
        let boxes = flat
            .as_chunks::<4>()
            .0
            .iter()
            .copied()
            .map(BoundingBox::from)
            .collect();
        let scores = read::primitive_list_at::<f32>(read::child(column, "scores")?, 0)?;
        let labels = read::primitive_list_at::<u32>(read::child(column, "labels")?, 0)?;
        Self::new(boxes, scores, labels)
    }
}

super::impl_from_payload!(Detections);

/// Landmark sets with per-point confidence — `std/vision/v1/Keypoints`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Keypoints {
    /// The points.
    pub points: Vec<Keypoint>,
    /// The per-point confidences, index-aligned with `points`.
    pub scores: Vec<f32>,
    /// The per-point landmark ids, index-aligned with `points`.
    pub labels: Vec<u32>,
}

impl Keypoints {
    /// Keypoints whose three lists are the same length.
    ///
    /// # Errors
    ///
    /// [`DataError::ColumnLengthMismatch`] when they are not.
    pub fn new(points: Vec<Keypoint>, scores: Vec<f32>, labels: Vec<u32>) -> Result<Self> {
        let value = Self {
            points,
            scores,
            labels,
        };
        value.check_alignment()?;
        Ok(value)
    }

    /// How many points the message carries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Whether the message carries no points.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Checks that the three index-aligned lists agree on length.
    ///
    /// # Errors
    ///
    /// [`DataError::ColumnLengthMismatch`].
    pub fn check_alignment(&self) -> Result<()> {
        check_three(
            ("points", self.points.len()),
            ("scores", self.scores.len()),
            ("labels", self.labels.len()),
        )
    }

    /// The columnar layout of this type.
    #[must_use]
    pub fn layout() -> DataType {
        DataType::strukt([
            Field::required(
                "points",
                DataType::list(Field::required(
                    "xy",
                    DataType::fixed_size_list(Field::required("v", DataType::Float32), 2),
                )),
            ),
            Field::required(
                "scores",
                DataType::list(Field::required("score", DataType::Float32)),
            ),
            Field::required(
                "labels",
                DataType::list(Field::required("label", DataType::UInt32)),
            ),
        ])
    }
}

impl AstrsMessage for Keypoints {
    const URN: &'static str = "std/vision/v1/Keypoints";

    fn data_type() -> DataType {
        Self::layout()
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        self.check_alignment()?;
        let flat: Vec<f32> = self
            .points
            .iter()
            .flat_map(|value| value.to_array())
            .collect();
        let column = build::structure(vec![
            (
                "points",
                build::fixed_tuple_lists::<f32>("xy", "v", 2, &[&flat])?,
            ),
            (
                "scores",
                build::primitive_lists::<f32>("score", &[&self.scores])?,
            ),
            (
                "labels",
                build::primitive_lists::<u32>("label", &[&self.labels])?,
            ),
        ])?;
        Ok(RecordBatch::from_payload(column))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        let (flat, width) = read::fixed_tuple_list_at::<f32>(read::child(column, "points")?, 0)?;
        if width != 2 {
            return Err(DataError::ChildLengthMismatch {
                expected: 2,
                actual: width,
            });
        }
        // As `Detections` above: the width check makes `as_chunks::<2>`
        // exact, and each chunk is the `[f32; 2]` `Keypoint`'s `From` takes.
        let points = flat
            .as_chunks::<2>()
            .0
            .iter()
            .copied()
            .map(Keypoint::from)
            .collect();
        let scores = read::primitive_list_at::<f32>(read::child(column, "scores")?, 0)?;
        let labels = read::primitive_list_at::<u32>(read::child(column, "labels")?, 0)?;
        Self::new(points, scores, labels)
    }
}

super::impl_from_payload!(Keypoints);

/// A per-pixel label plane — `std/vision/v1/Mask`, row-major.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mask {
    /// The plane's width in pixels.
    pub width: u32,
    /// The plane's height in pixels.
    pub height: u32,
    /// `width * height` labels, row-major.
    pub data: Vec<u16>,
}

impl Mask {
    /// A mask whose data length matches its dimensions.
    ///
    /// # Errors
    ///
    /// [`DataError::ChildLengthMismatch`] when `data.len() != width * height`.
    pub fn new(width: u32, height: u32, data: Vec<u16>) -> Result<Self> {
        let value = Self {
            width,
            height,
            data,
        };
        value.check_dimensions()?;
        Ok(value)
    }

    /// The label at `(x, y)`, or `None` outside the plane.
    #[must_use]
    pub fn label_at(&self, x: u32, y: u32) -> Option<u16> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = usize::try_from(y)
            .ok()?
            .checked_mul(usize::try_from(self.width).ok()?)?
            + usize::try_from(x).ok()?;
        self.data.get(index).copied()
    }

    /// Checks that the data length matches the declared dimensions.
    ///
    /// # Errors
    ///
    /// [`DataError::ChildLengthMismatch`].
    pub fn check_dimensions(&self) -> Result<()> {
        let expected = expected_cells(self.width, self.height);
        if self.data.len() == expected {
            Ok(())
        } else {
            Err(DataError::ChildLengthMismatch {
                expected,
                actual: self.data.len(),
            })
        }
    }

    /// The columnar layout of this type.
    #[must_use]
    pub fn layout() -> DataType {
        DataType::strukt([
            Field::required("width", DataType::UInt32),
            Field::required("height", DataType::UInt32),
            Field::required(
                "data",
                DataType::list(Field::required("label", DataType::UInt16)),
            ),
        ])
    }
}

impl AstrsMessage for Mask {
    const URN: &'static str = "std/vision/v1/Mask";

    fn data_type() -> DataType {
        Self::layout()
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        self.check_dimensions()?;
        let column = build::structure(vec![
            ("width", build::primitive::<u32>(&[self.width])),
            ("height", build::primitive::<u32>(&[self.height])),
            (
                "data",
                build::primitive_lists::<u16>("label", &[&self.data])?,
            ),
        ])?;
        Ok(RecordBatch::from_payload(column))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        let width = read::primitive_at::<u32>(read::child(column, "width")?, 0)?;
        let height = read::primitive_at::<u32>(read::child(column, "height")?, 0)?;
        let data = read::primitive_list_at::<u16>(read::child(column, "data")?, 0)?;
        Self::new(width, height, data)
    }
}

super::impl_from_payload!(Mask);

/// The cell count `width * height` implies, saturating rather than wrapping.
fn expected_cells(width: u32, height: u32) -> usize {
    usize::try_from(u64::from(width) * u64::from(height)).unwrap_or(usize::MAX)
}

/// Checks that three index-aligned lists agree on length.
///
/// Each argument is the list's `(name, length)`, so the error names the
/// column that disagreed rather than only its position.
fn check_three(first: (&str, usize), second: (&str, usize), third: (&str, usize)) -> Result<()> {
    if first.1 != second.1 {
        return Err(DataError::ColumnLengthMismatch {
            index: 1,
            name: second.0.to_owned(),
            expected: first.1,
            actual: second.1,
        });
    }
    if first.1 != third.1 {
        return Err(DataError::ColumnLengthMismatch {
            index: 2,
            name: third.0.to_owned(),
            expected: first.1,
            actual: third.1,
        });
    }
    Ok(())
}

/// The `boxes` column of a `Detections` payload, without decoding the rest.
///
/// The shape a tracker wants: it needs the geometry every frame and the
/// labels only when a track is born.
///
/// # Errors
///
/// [`DataError`] when the payload is not a `Detections` layout.
pub fn detection_boxes(batch: &RecordBatch) -> Result<Vec<BoundingBox>> {
    let column = single_row_column(batch)?;
    let (flat, width) = read::fixed_tuple_list_at::<f32>(read::child(column, "boxes")?, 0)?;
    if width != 4 {
        return Err(DataError::ChildLengthMismatch {
            expected: 4,
            actual: width,
        });
    }
    Ok(flat
        .as_chunks::<4>()
        .0
        .iter()
        .copied()
        .map(BoundingBox::from)
        .collect())
}

/// The raw `data` column of a `Mask` payload, without copying the dimensions.
///
/// # Errors
///
/// [`DataError`] when the payload is not a `Mask` layout.
pub fn mask_labels(batch: &RecordBatch) -> Result<ArrayRef> {
    let column = single_row_column(batch)?;
    Ok(std::sync::Arc::clone(read::child(column, "data")?))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::message::assert_registry_layout;

    fn detections() -> Detections {
        Detections::new(
            vec![
                BoundingBox::new(0.0, 0.0, 10.0, 20.0),
                BoundingBox::new(5.0, 5.0, 1.0, 2.0),
            ],
            vec![0.93, 0.41],
            vec![7, 12],
        )
        .unwrap()
    }

    #[test]
    fn every_vision_type_conforms_to_the_registry() {
        assert_registry_layout::<Detections>().unwrap();
        assert_registry_layout::<Keypoints>().unwrap();
        assert_registry_layout::<Mask>().unwrap();
    }

    #[test]
    fn detections_round_trip() {
        let value = detections();
        let batch = value.to_record_batch().unwrap();
        assert_eq!(batch.num_rows(), 1);
        let decoded = Detections::from_record_batch(&batch).unwrap();
        assert_eq!(decoded, value);
        assert_eq!(decoded.len(), 2);
        assert!(!decoded.is_empty());
        assert_eq!(decoded.boxes[0].area(), 200.0);
        assert_eq!(detection_boxes(&batch).unwrap(), value.boxes);
    }

    #[test]
    fn empty_detections_round_trip() {
        let value = Detections::empty();
        assert!(value.is_empty());
        let batch = value.to_record_batch().unwrap();
        assert_eq!(Detections::from_record_batch(&batch).unwrap(), value);
    }

    #[test]
    fn misaligned_detection_lists_are_refused() {
        let error =
            Detections::new(vec![BoundingBox::default()], vec![0.5, 0.6], vec![1]).unwrap_err();
        assert!(matches!(error, DataError::ColumnLengthMismatch { .. }));

        assert!(
            Detections::new(vec![BoundingBox::default()], vec![0.5], vec![1, 2]).is_err(),
            "labels must align too"
        );
    }

    #[test]
    fn keypoints_round_trip() {
        let value = Keypoints::new(
            vec![Keypoint::new(1.0, 2.0), Keypoint::new(3.0, 4.0)],
            vec![0.5, 0.25],
            vec![0, 1],
        )
        .unwrap();
        let batch = value.to_record_batch().unwrap();
        assert_eq!(Keypoints::from_record_batch(&batch).unwrap(), value);
        assert_eq!(value.len(), 2);
        assert!(!value.is_empty());
        assert_eq!(Keypoint::from([9.0, 8.0]).to_array(), [9.0, 8.0]);
        assert!(Keypoints::new(vec![Keypoint::default()], vec![], vec![]).is_err());
    }

    #[test]
    fn masks_round_trip_and_check_their_dimensions() {
        let value = Mask::new(3, 2, vec![1, 2, 3, 4, 5, 6]).unwrap();
        let batch = value.to_record_batch().unwrap();
        assert_eq!(Mask::from_record_batch(&batch).unwrap(), value);
        assert_eq!(value.label_at(0, 0), Some(1));
        assert_eq!(value.label_at(2, 1), Some(6));
        assert_eq!(value.label_at(3, 0), None);
        assert_eq!(value.label_at(0, 2), None);
        assert_eq!(mask_labels(&batch).unwrap().len(), 1);

        let error = Mask::new(3, 2, vec![1, 2, 3]).unwrap_err();
        assert!(matches!(error, DataError::ChildLengthMismatch { .. }));
        assert!(Mask::new(0, 0, Vec::new()).is_ok());
    }

    #[test]
    fn bounding_boxes_convert_both_ways() {
        let value = BoundingBox::from([1.0, 2.0, 3.0, 4.0]);
        assert_eq!(value.to_array(), [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(value.area(), 12.0);
        assert_eq!(BoundingBox::new(0.0, 0.0, -1.0, 5.0).area(), 0.0);
        assert_eq!(BoundingBox::default().area(), 0.0);
    }

    #[test]
    fn a_wrong_layout_is_refused() {
        let mask = Mask::new(1, 1, vec![0]).unwrap().to_record_batch().unwrap();
        assert!(Detections::from_record_batch(&mask).is_err());
        assert!(Keypoints::from_record_batch(&mask).is_err());
        assert!(detection_boxes(&mask).is_err());

        let detections = detections().to_record_batch().unwrap();
        assert!(Mask::from_record_batch(&detections).is_err());
        assert!(
            Keypoints::from_record_batch(&detections).is_err(),
            "four-wide boxes are not two-wide points"
        );
    }
}
