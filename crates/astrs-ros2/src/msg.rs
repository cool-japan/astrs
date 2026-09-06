//! Every ROS 2 message type this crate can carry, under one path.
//!
//! Two generated trees feed a bridged system — `astrs-idl`'s pre-generated
//! `common_interfaces` set (`std_msgs`, `geometry_msgs`, `sensor_msgs`,
//! `nav_msgs`, `std_srvs`, `builtin_interfaces`) and this crate's own
//! ([`crate::interfaces`]: `rcl_interfaces`, `action_msgs`,
//! `unique_identifier_msgs`, `rosgraph_msgs`, `example_interfaces`). A
//! program bridging `/scan` should not have to know which crate generated
//! `sensor_msgs/msg/LaserScan`, so this module re-exports both under one
//! name and implements [`MessageType`](crate::MessageType) for every type
//! in them.
//!
//! # Why the impls live here rather than beside the types
//!
//! `MessageType` is this crate's trait and `LaserScan` is `astrs-idl`'s
//! type; only a crate that owns one of the two can write the impl, and this
//! is it. Doing it in one place also makes the coverage checkable: the test
//! at the bottom of this module asserts that every re-exported type
//! implements the trait, so a package added to either tree cannot be
//! silently left out.
//!
//! # Example
//!
//! ```
//! use astrs_ros2::MessageType;
//! use astrs_ros2::msg::sensor_msgs::LaserScan;
//!
//! assert_eq!(
//!     <LaserScan as MessageType>::DDS_NAME,
//!     "sensor_msgs::msg::dds_::LaserScan_"
//! );
//! ```

pub use crate::idl::generated::{
    builtin_interfaces, geometry_msgs, nav_msgs, sensor_msgs, std_msgs, std_srvs,
};
pub use crate::interfaces::{
    action_msgs, example_interfaces, rcl_interfaces, rosgraph_msgs, unique_identifier_msgs,
};

crate::ros_message!(builtin_interfaces::Duration, builtin_interfaces::Time);

crate::ros_message!(
    geometry_msgs::Accel,
    geometry_msgs::AccelStamped,
    geometry_msgs::AccelWithCovariance,
    geometry_msgs::AccelWithCovarianceStamped,
    geometry_msgs::Inertia,
    geometry_msgs::InertiaStamped,
    geometry_msgs::Point,
    geometry_msgs::Point32,
    geometry_msgs::PointStamped,
    geometry_msgs::Polygon,
    geometry_msgs::PolygonStamped,
    geometry_msgs::Pose,
    geometry_msgs::PoseArray,
    geometry_msgs::PoseStamped,
    geometry_msgs::PoseWithCovariance,
    geometry_msgs::PoseWithCovarianceStamped,
    geometry_msgs::Quaternion,
    geometry_msgs::QuaternionStamped,
    geometry_msgs::Transform,
    geometry_msgs::TransformStamped,
    geometry_msgs::Twist,
    geometry_msgs::TwistStamped,
    geometry_msgs::TwistWithCovariance,
    geometry_msgs::TwistWithCovarianceStamped,
    geometry_msgs::Vector3,
    geometry_msgs::Vector3Stamped,
    geometry_msgs::Wrench,
    geometry_msgs::WrenchStamped,
);

crate::ros_message!(
    nav_msgs::GridCells,
    nav_msgs::MapMetaData,
    nav_msgs::OccupancyGrid,
    nav_msgs::Odometry,
    nav_msgs::Path,
);

crate::ros_message!(
    sensor_msgs::CameraInfo,
    sensor_msgs::ChannelFloat32,
    sensor_msgs::CompressedImage,
    sensor_msgs::FluidPressure,
    sensor_msgs::Illuminance,
    sensor_msgs::Image,
    sensor_msgs::Imu,
    sensor_msgs::JointState,
    sensor_msgs::Joy,
    sensor_msgs::JoyFeedback,
    sensor_msgs::JoyFeedbackArray,
    sensor_msgs::LaserScan,
    sensor_msgs::MagneticField,
    sensor_msgs::NavSatFix,
    sensor_msgs::NavSatStatus,
    sensor_msgs::PointCloud2,
    sensor_msgs::PointField,
    sensor_msgs::Range,
    sensor_msgs::RegionOfInterest,
    sensor_msgs::RelativeHumidity,
    sensor_msgs::Temperature,
    sensor_msgs::TimeReference,
);

crate::ros_message!(
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
);

crate::ros_message!(
    std_srvs::EmptyRequest,
    std_srvs::EmptyResponse,
    std_srvs::SetBoolRequest,
    std_srvs::SetBoolResponse,
    std_srvs::TriggerRequest,
    std_srvs::TriggerResponse,
);

crate::ros_message!(
    action_msgs::CancelGoalRequest,
    action_msgs::CancelGoalResponse,
    action_msgs::GoalInfo,
    action_msgs::GoalStatus,
    action_msgs::GoalStatusArray,
);

crate::ros_message!(
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
);

crate::ros_message!(
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
);

crate::ros_message!(rosgraph_msgs::Clock);

crate::ros_message!(unique_identifier_msgs::UUID);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::MessageType;
    use crate::names::mangle::{demangle_type_name, mangle_type_name};

    /// Every type whose `MessageType` impl this module owns, as
    /// `(TYPE_NAME, DDS_NAME)`.
    ///
    /// Assembled by hand from the `ros_message!` invocations above so that
    /// the coverage assertions below have something to iterate. Adding a
    /// type to an invocation without adding it here makes
    /// [`the_two_trees_are_fully_covered`] fail on the count.
    fn declared() -> Vec<(&'static str, &'static str)> {
        fn entry<T: MessageType>() -> (&'static str, &'static str) {
            (T::TYPE_NAME, T::DDS_NAME)
        }
        vec![
            entry::<builtin_interfaces::Duration>(),
            entry::<builtin_interfaces::Time>(),
            entry::<geometry_msgs::Accel>(),
            entry::<geometry_msgs::AccelStamped>(),
            entry::<geometry_msgs::AccelWithCovariance>(),
            entry::<geometry_msgs::AccelWithCovarianceStamped>(),
            entry::<geometry_msgs::Inertia>(),
            entry::<geometry_msgs::InertiaStamped>(),
            entry::<geometry_msgs::Point>(),
            entry::<geometry_msgs::Point32>(),
            entry::<geometry_msgs::PointStamped>(),
            entry::<geometry_msgs::Polygon>(),
            entry::<geometry_msgs::PolygonStamped>(),
            entry::<geometry_msgs::Pose>(),
            entry::<geometry_msgs::PoseArray>(),
            entry::<geometry_msgs::PoseStamped>(),
            entry::<geometry_msgs::PoseWithCovariance>(),
            entry::<geometry_msgs::PoseWithCovarianceStamped>(),
            entry::<geometry_msgs::Quaternion>(),
            entry::<geometry_msgs::QuaternionStamped>(),
            entry::<geometry_msgs::Transform>(),
            entry::<geometry_msgs::TransformStamped>(),
            entry::<geometry_msgs::Twist>(),
            entry::<geometry_msgs::TwistStamped>(),
            entry::<geometry_msgs::TwistWithCovariance>(),
            entry::<geometry_msgs::TwistWithCovarianceStamped>(),
            entry::<geometry_msgs::Vector3>(),
            entry::<geometry_msgs::Vector3Stamped>(),
            entry::<geometry_msgs::Wrench>(),
            entry::<geometry_msgs::WrenchStamped>(),
            entry::<nav_msgs::GridCells>(),
            entry::<nav_msgs::MapMetaData>(),
            entry::<nav_msgs::OccupancyGrid>(),
            entry::<nav_msgs::Odometry>(),
            entry::<nav_msgs::Path>(),
            entry::<sensor_msgs::CameraInfo>(),
            entry::<sensor_msgs::ChannelFloat32>(),
            entry::<sensor_msgs::CompressedImage>(),
            entry::<sensor_msgs::FluidPressure>(),
            entry::<sensor_msgs::Illuminance>(),
            entry::<sensor_msgs::Image>(),
            entry::<sensor_msgs::Imu>(),
            entry::<sensor_msgs::JointState>(),
            entry::<sensor_msgs::Joy>(),
            entry::<sensor_msgs::JoyFeedback>(),
            entry::<sensor_msgs::JoyFeedbackArray>(),
            entry::<sensor_msgs::LaserScan>(),
            entry::<sensor_msgs::MagneticField>(),
            entry::<sensor_msgs::NavSatFix>(),
            entry::<sensor_msgs::NavSatStatus>(),
            entry::<sensor_msgs::PointCloud2>(),
            entry::<sensor_msgs::PointField>(),
            entry::<sensor_msgs::Range>(),
            entry::<sensor_msgs::RegionOfInterest>(),
            entry::<sensor_msgs::RelativeHumidity>(),
            entry::<sensor_msgs::Temperature>(),
            entry::<sensor_msgs::TimeReference>(),
            entry::<std_msgs::Bool>(),
            entry::<std_msgs::Byte>(),
            entry::<std_msgs::ByteMultiArray>(),
            entry::<std_msgs::Char>(),
            entry::<std_msgs::ColorRGBA>(),
            entry::<std_msgs::Empty>(),
            entry::<std_msgs::Float32>(),
            entry::<std_msgs::Float32MultiArray>(),
            entry::<std_msgs::Float64>(),
            entry::<std_msgs::Float64MultiArray>(),
            entry::<std_msgs::Header>(),
            entry::<std_msgs::Int8>(),
            entry::<std_msgs::Int8MultiArray>(),
            entry::<std_msgs::Int16>(),
            entry::<std_msgs::Int16MultiArray>(),
            entry::<std_msgs::Int32>(),
            entry::<std_msgs::Int32MultiArray>(),
            entry::<std_msgs::Int64>(),
            entry::<std_msgs::Int64MultiArray>(),
            entry::<std_msgs::MultiArrayDimension>(),
            entry::<std_msgs::MultiArrayLayout>(),
            entry::<std_msgs::String>(),
            entry::<std_msgs::UInt8>(),
            entry::<std_msgs::UInt8MultiArray>(),
            entry::<std_msgs::UInt16>(),
            entry::<std_msgs::UInt16MultiArray>(),
            entry::<std_msgs::UInt32>(),
            entry::<std_msgs::UInt32MultiArray>(),
            entry::<std_msgs::UInt64>(),
            entry::<std_msgs::UInt64MultiArray>(),
            entry::<std_srvs::EmptyRequest>(),
            entry::<std_srvs::EmptyResponse>(),
            entry::<std_srvs::SetBoolRequest>(),
            entry::<std_srvs::SetBoolResponse>(),
            entry::<std_srvs::TriggerRequest>(),
            entry::<std_srvs::TriggerResponse>(),
            entry::<action_msgs::CancelGoalRequest>(),
            entry::<action_msgs::CancelGoalResponse>(),
            entry::<action_msgs::GoalInfo>(),
            entry::<action_msgs::GoalStatus>(),
            entry::<action_msgs::GoalStatusArray>(),
            entry::<example_interfaces::AddTwoIntsRequest>(),
            entry::<example_interfaces::AddTwoIntsResponse>(),
            entry::<example_interfaces::FibonacciFeedback>(),
            entry::<example_interfaces::FibonacciFeedbackMessage>(),
            entry::<example_interfaces::FibonacciGetResultRequest>(),
            entry::<example_interfaces::FibonacciGetResultResponse>(),
            entry::<example_interfaces::FibonacciGoal>(),
            entry::<example_interfaces::FibonacciResult>(),
            entry::<example_interfaces::FibonacciSendGoalRequest>(),
            entry::<example_interfaces::FibonacciSendGoalResponse>(),
            entry::<rcl_interfaces::DescribeParametersRequest>(),
            entry::<rcl_interfaces::DescribeParametersResponse>(),
            entry::<rcl_interfaces::FloatingPointRange>(),
            entry::<rcl_interfaces::GetParameterTypesRequest>(),
            entry::<rcl_interfaces::GetParameterTypesResponse>(),
            entry::<rcl_interfaces::GetParametersRequest>(),
            entry::<rcl_interfaces::GetParametersResponse>(),
            entry::<rcl_interfaces::IntegerRange>(),
            entry::<rcl_interfaces::ListParametersRequest>(),
            entry::<rcl_interfaces::ListParametersResponse>(),
            entry::<rcl_interfaces::ListParametersResult>(),
            entry::<rcl_interfaces::Log>(),
            entry::<rcl_interfaces::Parameter>(),
            entry::<rcl_interfaces::ParameterDescriptor>(),
            entry::<rcl_interfaces::ParameterEvent>(),
            entry::<rcl_interfaces::ParameterEventDescriptors>(),
            entry::<rcl_interfaces::ParameterType>(),
            entry::<rcl_interfaces::ParameterValue>(),
            entry::<rcl_interfaces::SetParametersAtomicallyRequest>(),
            entry::<rcl_interfaces::SetParametersAtomicallyResponse>(),
            entry::<rcl_interfaces::SetParametersRequest>(),
            entry::<rcl_interfaces::SetParametersResponse>(),
            entry::<rcl_interfaces::SetParametersResult>(),
            entry::<rosgraph_msgs::Clock>(),
            entry::<unique_identifier_msgs::UUID>(),
        ]
    }

    #[test]
    fn the_two_trees_are_fully_covered() {
        let types = declared();
        assert_eq!(
            types.len(),
            133,
            "a type was added to or removed from a `ros_message!` invocation \
             without updating this list"
        );
        let mut names: Vec<&str> = types.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "two types claim one ROS type name");
    }

    #[test]
    fn every_declared_type_names_itself_consistently() {
        for (ros, dds) in declared() {
            assert_eq!(
                mangle_type_name(ros).as_deref(),
                Some(dds),
                "{ros} does not mangle to {dds}"
            );
            assert_eq!(
                demangle_type_name(dds).as_deref(),
                Some(ros),
                "{dds} does not demangle to {ros}"
            );
        }
    }

    #[test]
    fn service_halves_carry_the_rosidl_suffix() {
        assert_eq!(
            <std_srvs::TriggerRequest as MessageType>::TYPE_NAME,
            "std_srvs/srv/Trigger_Request"
        );
        assert_eq!(
            <std_srvs::TriggerResponse as MessageType>::DDS_NAME,
            "std_srvs::srv::dds_::Trigger_Response_"
        );
    }

    #[test]
    fn action_wire_types_carry_the_action_namespace() {
        assert_eq!(
            <example_interfaces::FibonacciSendGoalRequest as MessageType>::TYPE_NAME,
            "example_interfaces/action/Fibonacci_SendGoal_Request"
        );
    }

    #[test]
    fn a_message_round_trips_through_cdr() {
        let value = std_msgs::String {
            data: "hello".to_owned(),
        };
        let octets = astrs_cdr::to_vec_ros2(&value).expect("encode");
        assert_eq!(
            astrs_cdr::from_bytes::<std_msgs::String>(&octets).expect("decode"),
            value
        );
    }
}
