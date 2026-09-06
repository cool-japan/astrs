# This message contains an uncompressed image.

std_msgs/Header header

uint32 height
uint32 width

# The legal values for encoding are in file src/image_encodings.cpp.
string encoding

uint8 is_bigendian
uint32 step
uint8[] data
