//! `std/vision/v1` layouts — detector outputs as parallel-array `Struct`s.
//!
//! All three types describe a *set* of per-detection or per-pixel values for
//! one message (one image's worth of detections, keypoints or mask), so each
//! field is a [`DataType::List`] rather than a single scalar — the count
//! varies frame to frame, and the URN does not parameterise it. `model` and
//! `labels` are accepted parameters that identify *which* model or label set
//! produced the values; they are graph metadata, not part of the byte shape,
//! so every layout here is a plain `fn() -> DataType`.

use crate::datatype::{DataType, Field, Schema};

/// `std/vision/v1/Detections` — bounding boxes with scores and labels.
///
/// `{boxes: List<FixedSizeList<Float32, 4>>, scores: List<Float32>, labels:
/// List<UInt32>}`. One row per detection across the three lists, index-
/// aligned: `boxes[i]`/`scores[i]`/`labels[i]` describe the same box. Each
/// box is `[x, y, width, height]` in image pixel coordinates — this is
/// exactly the `Detections { boxes: Vec<[f32; 4]>, scores: Vec<f32>, labels:
/// Vec<u32> }` shape from blueprint §9.1's node API example, laid out
/// column-wise.
///
/// ```
/// use astrs_data::urn::layouts::vision::detections_layout;
///
/// let layout = detections_layout();
/// assert_eq!(layout.children().len(), 3);
/// ```
#[must_use]
pub fn detections_layout() -> DataType {
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

/// The single-column [`Schema`] for a `std/vision/v1/Detections` payload.
#[must_use]
pub fn detections_schema() -> Schema {
    Schema::payload(detections_layout(), false)
}

/// `std/vision/v1/Keypoints` — landmark sets with per-point confidence.
///
/// `{points: List<FixedSizeList<Float32, 2>>, scores: List<Float32>, labels:
/// List<UInt32>}`. Each point is `[x, y]` in image pixel coordinates;
/// `labels` names which landmark each point is (nose, left-eye, …) as a
/// caller-defined id, index-aligned with `points`/`scores` like
/// [`detections_layout`].
#[must_use]
pub fn keypoints_layout() -> DataType {
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

/// The single-column [`Schema`] for a `std/vision/v1/Keypoints` payload.
#[must_use]
pub fn keypoints_schema() -> Schema {
    Schema::payload(keypoints_layout(), false)
}

/// `std/vision/v1/Mask` — a per-pixel label plane.
///
/// `{width: UInt32, height: UInt32, data: List<UInt16>}`, row-major,
/// `data.len() == width * height`. `UInt16` labels support up to 65,535
/// classes — ample for a segmentation model — while staying half the width
/// of a `UInt32` label column.
#[must_use]
pub fn mask_layout() -> DataType {
    DataType::strukt([
        Field::required("width", DataType::UInt32),
        Field::required("height", DataType::UInt32),
        Field::required(
            "data",
            DataType::list(Field::required("label", DataType::UInt16)),
        ),
    ])
}

/// The single-column [`Schema`] for a `std/vision/v1/Mask` payload.
#[must_use]
pub fn mask_schema() -> Schema {
    Schema::payload(mask_layout(), false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{
        Array, FixedSizeListArray, Float32Array, IntoArrayRef, ListArray, UInt16Array, UInt32Array,
    };

    #[test]
    fn detections_layout_has_three_index_aligned_lists() {
        let DataType::Struct(fields) = detections_layout() else {
            panic!("expected a struct");
        };
        assert_eq!(fields.len(), 3);
        let boxes = fields[0].data_type();
        let DataType::List(item) = boxes else {
            panic!("boxes should be a list");
        };
        assert_eq!(
            item.data_type(),
            &DataType::fixed_size_list(Field::required("v", DataType::Float32), 4)
        );
    }

    #[test]
    fn keypoints_layout_uses_2d_points() {
        let DataType::Struct(fields) = keypoints_layout() else {
            panic!("expected a struct");
        };
        let DataType::List(item) = fields[0].data_type() else {
            panic!("points should be a list");
        };
        assert_eq!(
            item.data_type(),
            &DataType::fixed_size_list(Field::required("v", DataType::Float32), 2)
        );
    }

    #[test]
    fn mask_layout_pairs_dimensions_with_a_flat_label_plane() {
        let layout = mask_layout();
        assert!(layout.layout_eq(&DataType::strukt([
            Field::required("width", DataType::UInt32),
            Field::required("height", DataType::UInt32),
            Field::required(
                "data",
                DataType::list(Field::required("label", DataType::UInt16))
            ),
        ])));
    }

    #[test]
    fn a_detections_row_builds_and_reads_back() {
        use crate::array::StructArray;

        let boxes_child =
            Float32Array::from_values([0.0, 0.0, 10.0, 20.0, 5.0, 5.0, 8.0, 8.0]).into_array_ref();
        let boxes = FixedSizeListArray::try_new(
            Field::required("v", DataType::Float32),
            4,
            boxes_child,
            None,
        )
        .unwrap()
        .into_array_ref();
        let boxes_list = ListArray::try_from_lengths(
            Field::required(
                "xywh",
                DataType::fixed_size_list(Field::required("v", DataType::Float32), 4),
            ),
            [2],
            boxes,
        )
        .unwrap()
        .into_array_ref();

        let scores = ListArray::try_from_lengths(
            Field::required("score", DataType::Float32),
            [2],
            Float32Array::from_values([0.9, 0.5]).into_array_ref(),
        )
        .unwrap()
        .into_array_ref();
        let labels = ListArray::try_from_lengths(
            Field::required("label", DataType::UInt32),
            [2],
            UInt32Array::from_values([1u32, 7]).into_array_ref(),
        )
        .unwrap()
        .into_array_ref();

        let DataType::Struct(fields) = detections_layout() else {
            panic!("expected a struct");
        };
        let row = StructArray::try_new(fields, vec![boxes_list, scores, labels], None).unwrap();
        assert_eq!(row.len(), 1, "one message, two detections inside it");
        assert_eq!(row.data_type(), &detections_layout());
    }

    #[test]
    fn a_mask_row_builds_and_reads_back() {
        use crate::array::{PrimitiveArray, StructArray};

        let width = PrimitiveArray::from_values([2u32]).into_array_ref();
        let height = PrimitiveArray::from_values([2u32]).into_array_ref();
        let data = ListArray::try_from_lengths(
            Field::required("label", DataType::UInt16),
            [4],
            UInt16Array::from_values([0u16, 1, 1, 0]).into_array_ref(),
        )
        .unwrap()
        .into_array_ref();

        let DataType::Struct(fields) = mask_layout() else {
            panic!("expected a struct");
        };
        let row = StructArray::try_new(fields, vec![width, height, data], None).unwrap();
        assert_eq!(row.len(), 1);
    }
}
