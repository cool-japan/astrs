# This represents a Vector3 with reference coordinate frame and timestamp.
#
# Note that this follows vector semantics with it always being free to translate.
# If you want to transform it using a translation, first convert it to a
# geometry_msgs/PointStamped.

std_msgs/Header header
Vector3 vector
