# This message holds a collection of N-dimensional points, which may
# contain additional information such as normals, intensity, etc.

std_msgs/Header header

uint32 height
uint32 width

PointField[] fields

bool is_bigendian
uint32 point_step
uint32 row_step
uint8[] data

bool is_dense
