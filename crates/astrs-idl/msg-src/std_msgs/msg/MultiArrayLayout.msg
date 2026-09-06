# This was originally provided as an example message.
# It is deprecated as of Foxy and to be removed in J-Turtle.
# Please use the MultiArrayLayout and its associated methods
# to unserialize data into arbitrary-dimensional arrays.

# The multiarray declares a generic multi-dimensional array of a
# particular data type.  Dimensions are ordered from outer most
# to inner most.

MultiArrayDimension[] dim  # Array of dimension properties
uint32 data_offset         # padding elements at front of data
