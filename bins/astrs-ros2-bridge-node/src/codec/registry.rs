//! The pre-generated message table: every type this build can bridge by name.
//!
//! §10.3 ships the `common_interfaces` set pre-generated, and `astrs-ros2`
//! declares [`astrs_ros2::MessageType`] for every type in both generated trees (its own
//! `interfaces/` and `astrs-idl`'s `generated/`). This module is the
//! *runtime* face of that set: one macro invocation per type, expanding into
//! a name comparison in [`lookup`] and an entry in [`known_types`].
//!
//! # Two lists, not one
//!
//! A type is registered as either `plain` or `stamped`, and the split is not
//! cosmetic: [`MessageCodec::stamped`] requires [`Stamped`], so a type
//! listed in the wrong half does not compile. That is deliberate — "does
//! this message carry a `std_msgs/Header`?" is a fact about the `.msg` file,
//! and encoding it in a place the compiler checks is worth more than a
//! runtime `if let Some(header)`.
//!
//! # Naming, for services and actions
//!
//! `astrs-idl` mints a request's ROS name as `pkg/srv/Service_Request` and
//! an action's endpoint types as `pkg/action/Action_SendGoal_Request` and
//! friends. [`request_type_name`], [`response_type_name`] and
//! [`action_endpoint_type_name`] build those spellings, so
//! [`crate::service`] and [`crate::action`] never format them by hand.

use astrs_ros2::msg::{
    action_msgs, builtin_interfaces, example_interfaces, geometry_msgs, nav_msgs, rcl_interfaces,
    rosgraph_msgs, sensor_msgs, std_msgs, std_srvs, unique_identifier_msgs,
};
use astrs_ros2::time::RosTime;

use crate::codec::{MessageCodec, RosNames, Stamped};

/// `astrs-tf`'s hand-written `tf2_msgs/msg/TFMessage`, under a module name
/// that matches its ROS package.
///
/// §10.6 gives `astrs-tf` "`/tf`-topic bridging both directions", and §10.3
/// leaves `tf2_msgs` out of the pre-generated `common_interfaces` bundle —
/// so the one type a `/tf` bridge needs lives in a third crate, is
/// hand-written, and (like every generated type) carries its two names as
/// inherent constants. [`impl_ros_names!`] treats it identically.
mod tf2_msgs {
    pub use astrs_tf::bridge::TfMessage;
}

/// Implement [`RosNames`] from a type's inherent `ROS_TYPE_NAME` /
/// `DDS_TYPE_NAME` constants.
///
/// Every `astrs-idl`-generated type has them, and so does `astrs-tf`'s
/// hand-written [`TfMessage`](astrs_tf::bridge::TfMessage) — which is the whole
/// reason this is a local trait rather than `astrs_ros2::MessageType`
/// (the orphan rule forbids implementing a foreign trait for a foreign
/// type).
macro_rules! impl_ros_names {
    ($( $ty:path ),* $(,)?) => {
        $(
            impl RosNames for $ty {
                const TYPE_NAME: &'static str = <$ty>::ROS_TYPE_NAME;
                const DDS_NAME: &'static str = <$ty>::DDS_TYPE_NAME;
            }
        )*
    };
}

/// Implement [`Stamped`] for every generated type whose first field is a
/// `std_msgs/Header`.
macro_rules! impl_stamped {
    ($( $ty:path ),* $(,)?) => {
        $(
            impl Stamped for $ty {
                fn ros_stamp(&self) -> RosTime {
                    RosTime::new(self.header.stamp.sec, self.header.stamp.nanosec)
                }
            }
        )*
    };
}

/// Build [`lookup`] and [`known_types`] from the two type lists.
macro_rules! message_registry {
    (
        plain: [ $( $plain:path ),* $(,)? ],
        stamped: [ $( $stamped:path ),* $(,)? ],
    ) => {
        /// The codec for a ROS 2 message type, by its ROS 2 name.
        ///
        /// `None` for a type this build was not generated with — which is
        /// where [`crate::resolve`] takes over, with the ament tree.
        #[must_use]
        pub fn lookup(ros_type_name: &str) -> Option<MessageCodec> {
            $(
                if ros_type_name == <$plain as RosNames>::TYPE_NAME {
                    return Some(MessageCodec::plain::<$plain>());
                }
            )*
            $(
                if ros_type_name == <$stamped as RosNames>::TYPE_NAME {
                    return Some(MessageCodec::stamped::<$stamped>());
                }
            )*
            None
        }

        /// Every ROS 2 message type this build can bridge, sorted.
        #[must_use]
        pub fn known_types() -> Vec<&'static str> {
            let mut names = vec![
                $( <$plain as RosNames>::TYPE_NAME, )*
                $( <$stamped as RosNames>::TYPE_NAME, )*
            ];
            names.sort_unstable();
            names
        }

        /// Every type registered as carrying a header, sorted.
        ///
        /// Exposed for the coverage test, and for `astrs ros2 topics`'s
        /// "does this topic carry a stamp" column.
        #[must_use]
        pub fn stamped_types() -> Vec<&'static str> {
            let mut names = vec![ $( <$stamped as RosNames>::TYPE_NAME, )* ];
            names.sort_unstable();
            names
        }
    };
}

impl_ros_names!(
    geometry_msgs::Accel,
    geometry_msgs::AccelWithCovariance,
    geometry_msgs::Inertia,
    geometry_msgs::Point,
    geometry_msgs::Point32,
    geometry_msgs::Polygon,
    geometry_msgs::Pose,
    geometry_msgs::PoseWithCovariance,
    geometry_msgs::Quaternion,
    geometry_msgs::Transform,
    geometry_msgs::Twist,
    geometry_msgs::TwistWithCovariance,
    geometry_msgs::Vector3,
    geometry_msgs::Wrench,
    nav_msgs::MapMetaData,
    sensor_msgs::ChannelFloat32,
    sensor_msgs::JoyFeedback,
    sensor_msgs::JoyFeedbackArray,
    sensor_msgs::NavSatStatus,
    sensor_msgs::PointField,
    sensor_msgs::RegionOfInterest,
    std_msgs::Bool,
    std_msgs::Byte,
    std_msgs::ByteMultiArray,
    std_msgs::Char,
    std_msgs::ColorRGBA,
    std_msgs::Empty,
    std_msgs::Float32,
    std_msgs::Float32MultiArray,
    std_msgs::Float64,
    std_msgs::Float64MultiArray,
    std_msgs::Header,
    std_msgs::Int8,
    std_msgs::Int8MultiArray,
    std_msgs::Int16,
    std_msgs::Int16MultiArray,
    std_msgs::Int32,
    std_msgs::Int32MultiArray,
    std_msgs::Int64,
    std_msgs::Int64MultiArray,
    std_msgs::MultiArrayDimension,
    std_msgs::MultiArrayLayout,
    std_msgs::String,
    std_msgs::UInt8,
    std_msgs::UInt8MultiArray,
    std_msgs::UInt16,
    std_msgs::UInt16MultiArray,
    std_msgs::UInt32,
    std_msgs::UInt32MultiArray,
    std_msgs::UInt64,
    std_msgs::UInt64MultiArray,
    std_srvs::EmptyRequest,
    std_srvs::EmptyResponse,
    std_srvs::SetBoolRequest,
    std_srvs::SetBoolResponse,
    std_srvs::TriggerRequest,
    std_srvs::TriggerResponse,
    action_msgs::CancelGoalRequest,
    action_msgs::CancelGoalResponse,
    action_msgs::GoalInfo,
    action_msgs::GoalStatus,
    action_msgs::GoalStatusArray,
    example_interfaces::AddTwoIntsRequest,
    example_interfaces::AddTwoIntsResponse,
    example_interfaces::FibonacciFeedback,
    example_interfaces::FibonacciFeedbackMessage,
    example_interfaces::FibonacciGetResultRequest,
    example_interfaces::FibonacciGetResultResponse,
    example_interfaces::FibonacciGoal,
    example_interfaces::FibonacciResult,
    example_interfaces::FibonacciSendGoalRequest,
    example_interfaces::FibonacciSendGoalResponse,
    rcl_interfaces::DescribeParametersRequest,
    rcl_interfaces::DescribeParametersResponse,
    rcl_interfaces::FloatingPointRange,
    rcl_interfaces::GetParameterTypesRequest,
    rcl_interfaces::GetParameterTypesResponse,
    rcl_interfaces::GetParametersRequest,
    rcl_interfaces::GetParametersResponse,
    rcl_interfaces::IntegerRange,
    rcl_interfaces::ListParametersRequest,
    rcl_interfaces::ListParametersResponse,
    rcl_interfaces::ListParametersResult,
    rcl_interfaces::Log,
    rcl_interfaces::Parameter,
    rcl_interfaces::ParameterDescriptor,
    rcl_interfaces::ParameterEvent,
    rcl_interfaces::ParameterEventDescriptors,
    rcl_interfaces::ParameterType,
    rcl_interfaces::ParameterValue,
    rcl_interfaces::SetParametersAtomicallyRequest,
    rcl_interfaces::SetParametersAtomicallyResponse,
    rcl_interfaces::SetParametersRequest,
    rcl_interfaces::SetParametersResponse,
    rcl_interfaces::SetParametersResult,
    rosgraph_msgs::Clock,
    unique_identifier_msgs::UUID,
    builtin_interfaces::Duration,
    builtin_interfaces::Time,
    tf2_msgs::TfMessage,
    geometry_msgs::AccelStamped,
    geometry_msgs::AccelWithCovarianceStamped,
    geometry_msgs::InertiaStamped,
    geometry_msgs::PointStamped,
    geometry_msgs::PolygonStamped,
    geometry_msgs::PoseArray,
    geometry_msgs::PoseStamped,
    geometry_msgs::PoseWithCovarianceStamped,
    geometry_msgs::QuaternionStamped,
    geometry_msgs::TransformStamped,
    geometry_msgs::TwistStamped,
    geometry_msgs::TwistWithCovarianceStamped,
    geometry_msgs::Vector3Stamped,
    geometry_msgs::WrenchStamped,
    nav_msgs::GridCells,
    nav_msgs::OccupancyGrid,
    nav_msgs::Odometry,
    nav_msgs::Path,
    sensor_msgs::CameraInfo,
    sensor_msgs::CompressedImage,
    sensor_msgs::FluidPressure,
    sensor_msgs::Illuminance,
    sensor_msgs::Image,
    sensor_msgs::Imu,
    sensor_msgs::JointState,
    sensor_msgs::Joy,
    sensor_msgs::LaserScan,
    sensor_msgs::MagneticField,
    sensor_msgs::NavSatFix,
    sensor_msgs::PointCloud2,
    sensor_msgs::Range,
    sensor_msgs::RelativeHumidity,
    sensor_msgs::Temperature,
    sensor_msgs::TimeReference,
);

impl_stamped!(
    geometry_msgs::AccelStamped,
    geometry_msgs::AccelWithCovarianceStamped,
    geometry_msgs::InertiaStamped,
    geometry_msgs::PointStamped,
    geometry_msgs::PolygonStamped,
    geometry_msgs::PoseArray,
    geometry_msgs::PoseStamped,
    geometry_msgs::PoseWithCovarianceStamped,
    geometry_msgs::QuaternionStamped,
    geometry_msgs::TransformStamped,
    geometry_msgs::TwistStamped,
    geometry_msgs::TwistWithCovarianceStamped,
    geometry_msgs::Vector3Stamped,
    geometry_msgs::WrenchStamped,
    nav_msgs::GridCells,
    nav_msgs::OccupancyGrid,
    nav_msgs::Odometry,
    nav_msgs::Path,
    sensor_msgs::CameraInfo,
    sensor_msgs::CompressedImage,
    sensor_msgs::FluidPressure,
    sensor_msgs::Illuminance,
    sensor_msgs::Image,
    sensor_msgs::Imu,
    sensor_msgs::JointState,
    sensor_msgs::Joy,
    sensor_msgs::LaserScan,
    sensor_msgs::MagneticField,
    sensor_msgs::NavSatFix,
    sensor_msgs::PointCloud2,
    sensor_msgs::Range,
    sensor_msgs::RelativeHumidity,
    sensor_msgs::Temperature,
    sensor_msgs::TimeReference,
);

message_registry! {
    plain: [
        geometry_msgs::Accel,
        geometry_msgs::AccelWithCovariance,
        geometry_msgs::Inertia,
        geometry_msgs::Point,
        geometry_msgs::Point32,
        geometry_msgs::Polygon,
        geometry_msgs::Pose,
        geometry_msgs::PoseWithCovariance,
        geometry_msgs::Quaternion,
        geometry_msgs::Transform,
        geometry_msgs::Twist,
        geometry_msgs::TwistWithCovariance,
        geometry_msgs::Vector3,
        geometry_msgs::Wrench,
        nav_msgs::MapMetaData,
        sensor_msgs::ChannelFloat32,
        sensor_msgs::JoyFeedback,
        sensor_msgs::JoyFeedbackArray,
        sensor_msgs::NavSatStatus,
        sensor_msgs::PointField,
        sensor_msgs::RegionOfInterest,
        std_msgs::Bool,
        std_msgs::Byte,
        std_msgs::ByteMultiArray,
        std_msgs::Char,
        std_msgs::ColorRGBA,
        std_msgs::Empty,
        std_msgs::Float32,
        std_msgs::Float32MultiArray,
        std_msgs::Float64,
        std_msgs::Float64MultiArray,
        std_msgs::Header,
        std_msgs::Int8,
        std_msgs::Int8MultiArray,
        std_msgs::Int16,
        std_msgs::Int16MultiArray,
        std_msgs::Int32,
        std_msgs::Int32MultiArray,
        std_msgs::Int64,
        std_msgs::Int64MultiArray,
        std_msgs::MultiArrayDimension,
        std_msgs::MultiArrayLayout,
        std_msgs::String,
        std_msgs::UInt8,
        std_msgs::UInt8MultiArray,
        std_msgs::UInt16,
        std_msgs::UInt16MultiArray,
        std_msgs::UInt32,
        std_msgs::UInt32MultiArray,
        std_msgs::UInt64,
        std_msgs::UInt64MultiArray,
        std_srvs::EmptyRequest,
        std_srvs::EmptyResponse,
        std_srvs::SetBoolRequest,
        std_srvs::SetBoolResponse,
        std_srvs::TriggerRequest,
        std_srvs::TriggerResponse,
        action_msgs::CancelGoalRequest,
        action_msgs::CancelGoalResponse,
        action_msgs::GoalInfo,
        action_msgs::GoalStatus,
        action_msgs::GoalStatusArray,
        example_interfaces::AddTwoIntsRequest,
        example_interfaces::AddTwoIntsResponse,
        example_interfaces::FibonacciFeedback,
        example_interfaces::FibonacciFeedbackMessage,
        example_interfaces::FibonacciGetResultRequest,
        example_interfaces::FibonacciGetResultResponse,
        example_interfaces::FibonacciGoal,
        example_interfaces::FibonacciResult,
        example_interfaces::FibonacciSendGoalRequest,
        example_interfaces::FibonacciSendGoalResponse,
        rcl_interfaces::DescribeParametersRequest,
        rcl_interfaces::DescribeParametersResponse,
        rcl_interfaces::FloatingPointRange,
        rcl_interfaces::GetParameterTypesRequest,
        rcl_interfaces::GetParameterTypesResponse,
        rcl_interfaces::GetParametersRequest,
        rcl_interfaces::GetParametersResponse,
        rcl_interfaces::IntegerRange,
        rcl_interfaces::ListParametersRequest,
        rcl_interfaces::ListParametersResponse,
        rcl_interfaces::ListParametersResult,
        rcl_interfaces::Log,
        rcl_interfaces::Parameter,
        rcl_interfaces::ParameterDescriptor,
        rcl_interfaces::ParameterEvent,
        rcl_interfaces::ParameterEventDescriptors,
        rcl_interfaces::ParameterType,
        rcl_interfaces::ParameterValue,
        rcl_interfaces::SetParametersAtomicallyRequest,
        rcl_interfaces::SetParametersAtomicallyResponse,
        rcl_interfaces::SetParametersRequest,
        rcl_interfaces::SetParametersResponse,
        rcl_interfaces::SetParametersResult,
        rosgraph_msgs::Clock,
        unique_identifier_msgs::UUID,
        builtin_interfaces::Duration,
        builtin_interfaces::Time,
        tf2_msgs::TfMessage,
    ],
    stamped: [
        geometry_msgs::AccelStamped,
        geometry_msgs::AccelWithCovarianceStamped,
        geometry_msgs::InertiaStamped,
        geometry_msgs::PointStamped,
        geometry_msgs::PolygonStamped,
        geometry_msgs::PoseArray,
        geometry_msgs::PoseStamped,
        geometry_msgs::PoseWithCovarianceStamped,
        geometry_msgs::QuaternionStamped,
        geometry_msgs::TransformStamped,
        geometry_msgs::TwistStamped,
        geometry_msgs::TwistWithCovarianceStamped,
        geometry_msgs::Vector3Stamped,
        geometry_msgs::WrenchStamped,
        nav_msgs::GridCells,
        nav_msgs::OccupancyGrid,
        nav_msgs::Odometry,
        nav_msgs::Path,
        sensor_msgs::CameraInfo,
        sensor_msgs::CompressedImage,
        sensor_msgs::FluidPressure,
        sensor_msgs::Illuminance,
        sensor_msgs::Image,
        sensor_msgs::Imu,
        sensor_msgs::JointState,
        sensor_msgs::Joy,
        sensor_msgs::LaserScan,
        sensor_msgs::MagneticField,
        sensor_msgs::NavSatFix,
        sensor_msgs::PointCloud2,
        sensor_msgs::Range,
        sensor_msgs::RelativeHumidity,
        sensor_msgs::Temperature,
        sensor_msgs::TimeReference,
    ],
}

/// The ROS 2 name of a service's request type.
///
/// `example_interfaces/srv/AddTwoInts` → `example_interfaces/srv/AddTwoInts_Request`.
#[must_use]
pub fn request_type_name(service_type: &str) -> String {
    format!("{service_type}_Request")
}

/// The ROS 2 name of a service's response type.
#[must_use]
pub fn response_type_name(service_type: &str) -> String {
    format!("{service_type}_Response")
}

/// The ROS 2 name of one of an action's synthesized wire types.
///
/// `suffix` is `"SendGoal_Request"`, `"GetResult_Response"`,
/// `"FeedbackMessage"` and so on — exactly what `astrs-idl`'s
/// `codegen/action.rs` appends.
#[must_use]
pub fn action_endpoint_type_name(action_type: &str, suffix: &str) -> String {
    format!("{action_type}_{suffix}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_blueprints_own_example_type_resolves() {
        let codec = lookup("sensor_msgs/msg/LaserScan").expect("§10.5's example type");
        assert_eq!(codec.ros_type_name, "sensor_msgs/msg/LaserScan");
        assert!(codec.has_header);
    }

    #[test]
    fn an_unknown_type_is_absent_rather_than_a_panic() {
        assert!(lookup("my_msgs/msg/Custom").is_none());
        assert!(lookup("").is_none());
        assert!(
            lookup("sensor_msgs/msg/laserscan").is_none(),
            "case matters"
        );
    }

    /// The coverage guard: every type `astrs-ros2` declares a
    /// [`MessageType`] for must be reachable by name from here, or a
    /// manifest naming it would fail at startup for no reason.
    #[test]
    fn every_declared_message_type_is_registered() {
        for name in known_types() {
            assert!(lookup(name).is_some(), "{name} is listed but not reachable");
        }
        assert!(
            known_types().len() >= 90,
            "the pre-generated set is ~130 types; found {}",
            known_types().len()
        );
    }

    #[test]
    fn the_table_names_each_type_once() {
        let mut names = known_types();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "a type is registered twice");
    }

    #[test]
    fn every_stamped_type_reports_a_header() {
        for name in stamped_types() {
            let codec = lookup(name).unwrap();
            assert!(codec.has_header, "{name}");
        }
    }

    #[test]
    fn a_codecs_dds_name_is_the_mangled_ros_name() {
        for name in known_types() {
            let codec = lookup(name).unwrap();
            assert_eq!(
                astrs_ros2::names::mangle::mangle_type_name(name).as_deref(),
                Some(codec.dds_type_name),
                "{name}"
            );
        }
    }

    #[test]
    fn every_urn_is_in_the_ros2_category() {
        for name in known_types() {
            let codec = lookup(name).unwrap();
            assert!(
                codec.urn.starts_with("std/ros2/v1/"),
                "{name} minted {}",
                codec.urn
            );
        }
    }

    #[test]
    fn a_service_types_two_halves_are_both_registered() {
        for service in [
            "std_srvs/srv/Empty",
            "std_srvs/srv/SetBool",
            "std_srvs/srv/Trigger",
            "example_interfaces/srv/AddTwoInts",
            "action_msgs/srv/CancelGoal",
        ] {
            assert!(
                lookup(&request_type_name(service)).is_some(),
                "{service} request"
            );
            assert!(
                lookup(&response_type_name(service)).is_some(),
                "{service} response"
            );
        }
    }

    #[test]
    fn an_actions_five_wire_types_are_all_registered() {
        let action = "example_interfaces/action/Fibonacci";
        for suffix in [
            "SendGoal_Request",
            "SendGoal_Response",
            "GetResult_Request",
            "GetResult_Response",
            "FeedbackMessage",
        ] {
            assert!(
                lookup(&action_endpoint_type_name(action, suffix)).is_some(),
                "{action} {suffix}"
            );
        }
    }

    /// §10.6's `/tf` bridging needs exactly one type, and it lives in
    /// `astrs-tf` rather than in either generated tree. A `ros2:` block
    /// naming it must therefore resolve like any other.
    #[test]
    fn the_tf_message_type_is_bridgeable() {
        let codec = lookup("tf2_msgs/msg/TFMessage").expect("§10.6's /tf type");
        assert_eq!(codec.dds_type_name, "tf2_msgs::msg::dds_::TFMessage_");
        assert_eq!(codec.urn, "std/ros2/v1/Tf2MsgsTFMessage");
        assert!(
            !codec.has_header,
            "TFMessage is a list of stamped transforms"
        );

        // …and it round-trips through the codec like every other type.
        let message = astrs_tf::bridge::TfMessage {
            transforms: Vec::new(),
        };
        let cdr = astrs_cdr::to_vec(&message, astrs_cdr::Encoding::ROS2).unwrap();
        let decoded = codec.decode(&cdr).unwrap();
        let recovered: astrs_tf::bridge::TfMessage =
            astrs_cdr::from_bytes_tolerant(&codec.encode(&decoded.batch).unwrap()).unwrap();
        assert_eq!(recovered, message);
    }

    #[test]
    fn the_name_builders_match_the_generated_spellings() {
        assert_eq!(
            request_type_name("std_srvs/srv/SetBool"),
            "std_srvs/srv/SetBool_Request"
        );
        assert_eq!(
            response_type_name("std_srvs/srv/SetBool"),
            "std_srvs/srv/SetBool_Response"
        );
        assert_eq!(
            action_endpoint_type_name("example_interfaces/action/Fibonacci", "FeedbackMessage"),
            "example_interfaces/action/Fibonacci_FeedbackMessage"
        );
    }
}
