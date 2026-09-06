"""A minimal Python ROS 2 launch file, used only to exercise
astrs-migrate's best-effort source-text skim -- never executed or parsed
(blueprint §18: this workspace is pure Rust). See tests/from_ros2.rs's
python_launch_file_yields_a_skeleton_with_unverified_candidates.
"""

from launch import LaunchDescription
from launch_ros.actions import Node


def generate_launch_description():
    return LaunchDescription([
        Node(
            package='demo_nodes_cpp',
            executable='talker',
            name='talker',
            namespace='chatter_demo',
        ),
    ])
