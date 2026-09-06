//! `/tf` and `/tf_static` topic conventions, in both directions (blueprint
//! §10.6).
//!
//! # The wire message: `tf2_msgs/msg/TFMessage`
//!
//! [`TfMessage`] (`{ transforms: Vec<TransformStamped> }`) is **hand-written**
//! rather than `astrs-idl`-generated: `tf2_msgs` is its own ROS package (the
//! `geometry2` repository), not part of the `common_interfaces` bundle
//! `astrs-idl` pre-generates (blueprint §10.3). Its five trait impls —
//! `astrs_idl::runtime::ColumnValue`, `astrs_data::AstrsMessage`,
//! `astrs_cdr::{CdrType, CdrSerialize, CdrDeserialize}` (via
//! [`astrs_cdr::cdr_struct!`]), `astrs_cdr::CdrDefault` — are written by hand
//! to the exact shape `astrs-idl`'s codegen would emit for an equivalent
//! `Vec<Struct>`-shaped single-field message (`nav_msgs/msg/Path`'s
//! `poses: Vec<PoseStamped>` is the closest pre-generated analogue), so a
//! future `astrs-idl` regeneration of `tf2_msgs` would produce a
//! byte-for-byte compatible type.
//!
//! # Topic conventions
//!
//! | Topic | Message | QoS (tf2_ros's own defaults) | Direction |
//! |---|---|---|---|
//! | [`TF_TOPIC`] (`/tf`) | [`TfMessage`] | reliable, volatile, `keep_last(100)` | dynamic frames |
//! | [`TF_STATIC_TOPIC`] (`/tf_static`) | [`TfMessage`] | reliable, **transient_local**, `keep_last(1)` | static frames |
//!
//! QoS objects themselves are outside this crate's layer (`astrs-ros2`/
//! `astrs-rtps`, blueprint §10.2/§10.4, own the actual publisher/subscriber
//! and RTPS QoS policy types) — the table above is the *contract* a bridge
//! built on those crates needs to honor, and [`TF_TOPIC`]/[`TF_STATIC_TOPIC`]
//! are the names it needs to agree on with the rest of the ROS graph.
//!
//! **Outbound** ([`TfMessage`]'s own constructor plus
//! [`StaticTransformAccumulator`]): a dynamic broadcaster wraps whatever
//! [`crate::interop::geometry_msgs::StampedTransform`]s it has for this
//! cycle and publishes them as-is — `tf2_ros::TransformBroadcaster` is
//! stateless. A **static** broadcaster is not: `tf2_ros::StaticTransformBroadcaster`
//! accumulates every static transform it has ever sent (keyed by child
//! frame) and re-publishes the *entire accumulated set* on every call,
//! because a `transient_local` subscriber joining late only ever receives
//! the single most recent sample on the topic — if that sample only
//! contained the newest edge, every earlier static edge would vanish for a
//! late joiner. [`StaticTransformAccumulator`] is that accumulator.
//!
//! **Inbound** ([`ingest_tf_message`]): converts each wire
//! `TransformStamped` in a received [`TfMessage`] into
//! [`crate::buffer::TransformBuffer::set_transform`] calls, tagging them
//! static or dynamic by *which topic they arrived on* — [`TfMessage`]
//! itself carries no such flag (matching real tf2: `/tf` and `/tf_static`
//! are only distinguished by name and QoS, never by message content).

use std::collections::HashMap;

use astrs_cdr::{cdr_default_impl, cdr_struct};
use astrs_data::array::{ArrayRef, IntoArrayRef, StructArray};
use astrs_data::{AstrsMessage, DataError, DataType, Field, RecordBatch};
use astrs_idl::generated::geometry_msgs::TransformStamped;
use astrs_idl::runtime::{
    ColumnValue, ITEM_FIELD, decode_list_column, encode_list_rows, struct_column,
};

use crate::buffer::TransformBuffer;
use crate::error::TfError;
use crate::interop::geometry_msgs::StampedTransform;

/// The dynamic-transform topic name every AstRS `/tf` bridge publishes to
/// and subscribes from.
pub const TF_TOPIC: &str = "/tf";

/// The static-transform topic name — reliable, **transient_local** QoS (see
/// this module's docs), so a late subscriber still receives every static
/// edge ever published.
pub const TF_STATIC_TOPIC: &str = "/tf_static";

cdr_struct! {
    /// `tf2_msgs/msg/TFMessage` — hand-written; see this module's docs for
    /// why it is not `astrs-idl`-generated.
    #[derive(Debug, Clone, PartialEq)]
    pub struct TfMessage {
        /// The transforms carried in this message.
        pub transforms: ::std::vec::Vec<TransformStamped>,
    }
}
cdr_default_impl!(TfMessage { transforms });

impl ::core::default::Default for TfMessage {
    /// Delegates to `CdrDefault::cdr_default` — the empty transform list —
    /// matching `astrs-idl` codegen's own `Default`/`CdrDefault` pairing
    /// (see `emit_default_impl` in that crate's `codegen::emit`).
    fn default() -> Self {
        <Self as astrs_cdr::CdrDefault>::cdr_default()
    }
}

impl TfMessage {
    /// ROS 2 type name: `tf2_msgs/msg/TFMessage`.
    pub const ROS_TYPE_NAME: &'static str = "tf2_msgs/msg/TFMessage";
    /// The DDS-mangled type name (`rosidl`'s own convention) SEDP discovery
    /// announcements carry for this type.
    pub const DDS_TYPE_NAME: &'static str = "tf2_msgs::msg::dds_::TFMessage_";

    /// Wraps `transforms` as a `TfMessage`, converting each from the native
    /// [`StampedTransform`] representation.
    ///
    /// # Errors
    ///
    /// [`TfError::RosTimeRangeExceeded`] if any transform's stamp does not
    /// fit `builtin_interfaces/Time`.
    ///
    /// ```
    /// use astrs_tf::TfStamp;
    /// use astrs_tf::bridge::TfMessage;
    /// use astrs_tf::interop::geometry_msgs::StampedTransform;
    /// use astrs_tf::math::Isometry3;
    ///
    /// # fn main() -> Result<(), astrs_tf::TfError> {
    /// let message = TfMessage::from_stamped_transforms(&[StampedTransform::new(
    ///     "map", "odom", Isometry3::IDENTITY, TfStamp::from_nanos(0),
    /// )])?;
    /// assert_eq!(message.transforms.len(), 1);
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_stamped_transforms<'a>(
        transforms: impl IntoIterator<Item = &'a StampedTransform>,
    ) -> Result<Self, TfError> {
        let transforms = transforms
            .into_iter()
            .map(StampedTransform::try_into_transform_stamped)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { transforms })
    }
}

impl ColumnValue for TfMessage {
    fn value_data_type() -> DataType {
        DataType::strukt([Field::required(
            "transforms",
            DataType::list(Field::required(
                ITEM_FIELD,
                <TransformStamped as ColumnValue>::value_data_type(),
            )),
        )])
    }

    fn encode_column(values: &[Self]) -> astrs_data::Result<ArrayRef> {
        let transforms_column = encode_list_rows(values.iter().map(|v| v.transforms.as_slice()))?;
        let DataType::Struct(fields) = Self::value_data_type() else {
            return Err(DataError::type_mismatch(
                Self::value_data_type(),
                DataType::Null,
            ));
        };
        let strukt =
            StructArray::try_new_with_len(fields, vec![transforms_column], values.len(), None)?;
        Ok(strukt.into_array_ref())
    }

    fn decode_row(
        array: &ArrayRef,
        row: usize,
        field_name: &'static str,
    ) -> astrs_data::Result<Self> {
        let _ = field_name;
        let column = struct_column(array, 0, 1)?;
        let transforms = decode_list_column::<TransformStamped>(column, row, "transforms")?;
        Ok(Self { transforms })
    }
}

impl AstrsMessage for TfMessage {
    /// Minted mechanically by `astrs-idl`'s own rule (blueprint §10.3):
    /// `std/ros2/v1/<PackageUpperCamel><TypeName>` —
    /// `"tf2_msgs".to_upper_camel_case()` is `"Tf2Msgs"` (verified against
    /// `heck::ToUpperCamelCase` directly in this module's tests, not just
    /// asserted here) concatenated with `"TFMessage"`.
    const URN: &'static str = "std/ros2/v1/Tf2MsgsTFMessage";

    fn data_type() -> DataType {
        <Self as ColumnValue>::value_data_type()
    }

    fn to_record_batch(&self) -> astrs_data::Result<RecordBatch> {
        let array = <Self as ColumnValue>::encode_column(std::slice::from_ref(self))?;
        Ok(RecordBatch::from_payload(array))
    }

    fn from_record_batch(batch: &RecordBatch) -> astrs_data::Result<Self> {
        if batch.num_rows() != 1 {
            return Err(DataError::MessageRowCount {
                actual: batch.num_rows(),
            });
        }
        let column = batch
            .payload_column()
            .ok_or(DataError::ColumnCountMismatch {
                fields: 1,
                columns: 0,
            })?;
        <Self as ColumnValue>::decode_row(column, 0, Self::ROS_TYPE_NAME)
    }
}

/// Accumulates static transforms by child frame and snapshots the full set
/// — `tf2_ros::StaticTransformBroadcaster`'s behavior (see this module's
/// docs for why a static broadcaster cannot be stateless the way a dynamic
/// one is).
#[derive(Debug, Clone, Default)]
pub struct StaticTransformAccumulator {
    by_child: HashMap<String, StampedTransform>,
}

impl StaticTransformAccumulator {
    /// An empty accumulator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_child: HashMap::new(),
        }
    }

    /// Records (or replaces) the static transform for `transform.child_frame`.
    pub fn set(&mut self, transform: StampedTransform) {
        self.by_child
            .insert(transform.child_frame.clone(), transform);
    }

    /// [`StaticTransformAccumulator::set`] for every transform in `transforms`.
    pub fn set_many(&mut self, transforms: impl IntoIterator<Item = StampedTransform>) {
        for transform in transforms {
            self.set(transform);
        }
    }

    /// The number of distinct child frames currently accumulated.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_child.len()
    }

    /// `true` when nothing has been accumulated yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_child.is_empty()
    }

    /// Builds the [`TfMessage`] to (re-)publish on [`TF_STATIC_TOPIC`]: the
    /// full accumulated set, ordered by child frame name for a
    /// deterministic, reproducible wire encoding (accumulation order itself
    /// is a `HashMap` iteration order, which is not).
    ///
    /// # Errors
    ///
    /// [`TfError::RosTimeRangeExceeded`] if any accumulated transform's
    /// stamp does not fit `builtin_interfaces/Time`.
    pub fn snapshot(&self) -> Result<TfMessage, TfError> {
        let mut entries: Vec<&StampedTransform> = self.by_child.values().collect();
        entries.sort_by(|a, b| a.child_frame.cmp(&b.child_frame));
        TfMessage::from_stamped_transforms(entries)
    }
}

/// Ingests every transform in `message` into `buffer`, tagging all of them
/// static or dynamic by `is_static` — the caller's job, since a received
/// [`TfMessage`] carries no such flag itself (it is only ever known from
/// which topic, [`TF_TOPIC`] or [`TF_STATIC_TOPIC`], the message arrived
/// on; see this module's docs).
///
/// Stops at the first failure — a partially-ingested message leaves
/// `buffer` with whichever earlier transforms in `message.transforms`
/// already succeeded, matching
/// [`crate::buffer::TransformBuffer::set_transform`]'s own per-call
/// atomicity (each individual `set_transform` either fully applies or has
/// no effect).
///
/// # Errors
///
/// Whatever [`crate::buffer::TransformBuffer::set_transform`] returns for
/// the first transform that fails.
pub fn ingest_tf_message(
    buffer: &mut TransformBuffer,
    message: &TfMessage,
    is_static: bool,
) -> Result<(), TfError> {
    for wire in &message.transforms {
        let stamped = StampedTransform::from(wire);
        buffer.set_transform(
            &stamped.parent_frame,
            &stamped.child_frame,
            stamped.transform,
            stamped.stamp,
            is_static,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::math::{Isometry3, Vector3};
    use crate::time::TimePoint;
    use astrs_cdr::{CdrDefault, from_bytes, to_vec_ros2};
    use heck::ToUpperCamelCase;

    fn stamped(parent: &str, child: &str, x: f64, nanos: i64) -> StampedTransform {
        StampedTransform::new(
            parent,
            child,
            Isometry3::from_translation(Vector3::new(x, 0.0, 0.0)),
            crate::time::TfStamp::from_nanos(nanos),
        )
    }

    #[test]
    fn urn_matches_the_mechanical_minting_rule() {
        // Verified against the real rule (`heck::ToUpperCamelCase`) rather
        // than trusted by inspection — see the module docs and
        // `astrs_idl::naming::mint_urn`.
        let package = "tf2_msgs".to_upper_camel_case();
        assert_eq!(package, "Tf2Msgs");
        let expected = format!("std/ros2/v1/{package}TFMessage");
        assert_eq!(TfMessage::URN, expected);
    }

    #[test]
    fn ros_type_name_and_dds_type_name_follow_rosidl_conventions() {
        assert_eq!(TfMessage::ROS_TYPE_NAME, "tf2_msgs/msg/TFMessage");
        assert_eq!(TfMessage::DDS_TYPE_NAME, "tf2_msgs::msg::dds_::TFMessage_");
    }

    #[test]
    fn default_is_an_empty_transform_list() {
        assert_eq!(TfMessage::cdr_default(), TfMessage { transforms: vec![] });
        assert_eq!(TfMessage::default().transforms, Vec::new());
    }

    #[test]
    fn cdr_round_trips_a_single_transform() {
        let wire = stamped("map", "odom", 2.5, 1_000)
            .try_into_transform_stamped()
            .unwrap();
        let message = TfMessage {
            transforms: vec![wire],
        };
        let bytes = to_vec_ros2(&message).unwrap();
        assert_eq!(from_bytes::<TfMessage>(&bytes).unwrap(), message);
    }

    #[test]
    fn cdr_round_trips_an_empty_message() {
        let message = TfMessage { transforms: vec![] };
        let bytes = to_vec_ros2(&message).unwrap();
        assert_eq!(from_bytes::<TfMessage>(&bytes).unwrap(), message);
    }

    #[test]
    fn cdr_round_trips_several_transforms() {
        let transforms = vec![
            stamped("map", "odom", 1.0, 0)
                .try_into_transform_stamped()
                .unwrap(),
            stamped("odom", "base_link", 2.0, 1_000)
                .try_into_transform_stamped()
                .unwrap(),
        ];
        let message = TfMessage { transforms };
        let bytes = to_vec_ros2(&message).unwrap();
        assert_eq!(from_bytes::<TfMessage>(&bytes).unwrap(), message);
    }

    #[test]
    fn a_hand_derived_cdr_encoding_for_one_transform() {
        // Worked derivation (mirroring §10.1's "golden vector" ethos): a
        // `TfMessage` holding one `TransformStamped` with empty frame ids
        // and the identity transform, XCDR1 little-endian. Alignment is
        // counted from the first octet after the four-octet encapsulation
        // header (this crate's own "alignment origin" convention).
        //
        // encapsulation                                  [00 01 00 00]  (CDR_LE)
        // TfMessage.transforms: sequence<TransformStamped>
        //   length = 1                            rel[0:4)   01 00 00 00
        // TransformStamped.header.stamp (builtin_interfaces/Time)
        //   sec: i32 = 0                           rel[4:8)   00 00 00 00
        //   nanosec: u32 = 0                        rel[8:12)  00 00 00 00
        // TransformStamped.header.frame_id: string, IDL default "" ->
        // length (content + NUL) = 1                rel[12:16) 01 00 00 00
        //   content (just the NUL)                  rel[16:17) 00
        //   pad to the next 4-byte boundary (17->20) rel[17:20) 00 00 00
        // TransformStamped.child_frame_id: string, same shape
        //   length = 1                              rel[20:24) 01 00 00 00
        //   content (NUL)                            rel[24:25) 00
        //   pad to the next 8-byte boundary for the
        //   f64s that follow (25->32, seven bytes,
        //   not three — the string content shifted
        //   everything four bytes later than a bare
        //   `TransformStamped` would sit at, changing
        //   which alignment boundary is next)         rel[25:32) 00 x 7
        // TransformStamped.transform.translation (Vector3, IDL default
        // all-zero: no defaults in `Vector3.msg`)
        //   x, y, z: f64 = 0.0 each                  rel[32:56) 00 x 24
        // TransformStamped.transform.rotation (Quaternion) — `Quaternion.msg`
        // declares an explicit non-IDL-zero default, `float64 w 1`:
        //   x, y, z: f64 = 0.0 each                  rel[56:80) 00 x 24
        //   w: f64 = 1.0                             rel[80:88) 00 00 00 00 00 00 f0 3f
        let message = TfMessage {
            transforms: vec![TransformStamped::cdr_default()],
        };
        let bytes = to_vec_ros2(&message).unwrap();
        let mut expected = vec![0x00, 0x01, 0x00, 0x00]; // encapsulation
        expected.extend_from_slice(&[1, 0, 0, 0]); // transforms: sequence length = 1
        expected.extend_from_slice(&[0; 4]); // sec
        expected.extend_from_slice(&[0; 4]); // nanosec
        expected.extend_from_slice(&[1, 0, 0, 0]); // frame_id length
        expected.push(0); // frame_id NUL
        expected.extend_from_slice(&[0; 3]); // pad to the next 4-byte boundary
        expected.extend_from_slice(&[1, 0, 0, 0]); // child_frame_id length
        expected.push(0); // child_frame_id NUL
        expected.extend_from_slice(&[0; 7]); // pad to the next 8-byte boundary
        expected.extend_from_slice(&[0; 24]); // translation (3 x f64)
        expected.extend_from_slice(&[0; 24]); // rotation.{x,y,z}
        expected.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0xf0, 0x3f]); // rotation.w = 1.0
        assert_eq!(bytes.len(), 92);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn columnar_round_trip_through_a_record_batch() {
        let message = TfMessage {
            transforms: vec![
                stamped("map", "odom", 1.0, 0)
                    .try_into_transform_stamped()
                    .unwrap(),
                stamped("odom", "base_link", 2.0, 1_000)
                    .try_into_transform_stamped()
                    .unwrap(),
            ],
        };
        let batch = message.to_record_batch().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(TfMessage::from_record_batch(&batch).unwrap(), message);
    }

    #[test]
    fn static_accumulator_replaces_by_child_frame() {
        let mut accumulator = StaticTransformAccumulator::new();
        accumulator.set(stamped("map", "odom", 1.0, 0));
        accumulator.set(stamped("map", "odom", 2.0, 1_000)); // replaces.
        assert_eq!(accumulator.len(), 1);
        let snapshot = accumulator.snapshot().unwrap();
        assert_eq!(snapshot.transforms.len(), 1);
        assert_eq!(snapshot.transforms[0].transform.translation.x, 2.0);
    }

    #[test]
    fn static_accumulator_keeps_every_child_frame_across_updates() {
        // The behavior this module's docs explain: adding a *new* static
        // edge does not drop earlier ones — the whole point of
        // accumulating rather than forwarding one edge at a time.
        let mut accumulator = StaticTransformAccumulator::new();
        accumulator.set(stamped("map", "odom", 1.0, 0));
        accumulator.set(stamped("odom", "base_link", 2.0, 0));
        let snapshot = accumulator.snapshot().unwrap();
        assert_eq!(snapshot.transforms.len(), 2);
    }

    #[test]
    fn static_accumulator_snapshot_is_ordered_by_child_frame() {
        let mut accumulator = StaticTransformAccumulator::new();
        accumulator.set(stamped("map", "zeta", 1.0, 0));
        accumulator.set(stamped("map", "alpha", 2.0, 0));
        let snapshot = accumulator.snapshot().unwrap();
        let names: Vec<&str> = snapshot
            .transforms
            .iter()
            .map(|t| t.child_frame_id.as_str())
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);
    }

    #[test]
    fn empty_accumulator_snapshots_to_an_empty_message() {
        let accumulator = StaticTransformAccumulator::new();
        assert!(accumulator.is_empty());
        assert_eq!(
            accumulator.snapshot().unwrap(),
            TfMessage { transforms: vec![] }
        );
    }

    #[test]
    fn ingest_tf_message_populates_the_buffer_as_dynamic() {
        let mut buffer = TransformBuffer::new();
        let message = TfMessage {
            transforms: vec![
                stamped("map", "odom", 3.0, 0)
                    .try_into_transform_stamped()
                    .unwrap(),
            ],
        };
        ingest_tf_message(&mut buffer, &message, false).unwrap();
        assert_eq!(
            buffer.frame_kind("odom"),
            Some(crate::error::FrameKind::Dynamic)
        );
        let looked_up = buffer
            .lookup_transform("map", "odom", TimePoint::Latest)
            .unwrap();
        assert_eq!(looked_up.translation.x, 3.0);
    }

    #[test]
    fn ingest_tf_message_populates_the_buffer_as_static() {
        let mut buffer = TransformBuffer::new();
        let message = TfMessage {
            transforms: vec![
                stamped("map", "odom", 3.0, 0)
                    .try_into_transform_stamped()
                    .unwrap(),
            ],
        };
        ingest_tf_message(&mut buffer, &message, true).unwrap();
        assert_eq!(
            buffer.frame_kind("odom"),
            Some(crate::error::FrameKind::Static)
        );
    }

    #[test]
    fn ingest_tf_message_applies_every_transform_in_the_message() {
        let mut buffer = TransformBuffer::new();
        let message = TfMessage {
            transforms: vec![
                stamped("map", "odom", 1.0, 0)
                    .try_into_transform_stamped()
                    .unwrap(),
                stamped("odom", "base_link", 2.0, 0)
                    .try_into_transform_stamped()
                    .unwrap(),
            ],
        };
        ingest_tf_message(&mut buffer, &message, true).unwrap();
        let looked_up = buffer
            .lookup_transform("map", "base_link", TimePoint::Latest)
            .unwrap();
        assert_eq!(looked_up.translation.x, 3.0);
    }

    #[test]
    fn from_stamped_transforms_propagates_a_time_range_error() {
        let bad = StampedTransform::new(
            "map",
            "odom",
            Isometry3::IDENTITY,
            crate::time::TfStamp::MAX,
        );
        assert!(matches!(
            TfMessage::from_stamped_transforms([&bad]),
            Err(TfError::RosTimeRangeExceeded { .. })
        ));
    }
}
