# This message defines meta information for a camera, computed from calibration.

# Time of image acquisition, camera coordinate frame ID
std_msgs/Header header

# The image dimensions with which the camera was calibrated.
uint32 height
uint32 width

# The distortion model used. Supported models are listed in
# sensor_msgs/distortion_models.hpp.
string distortion_model

# The distortion parameters, size depending on the distortion model.
float64[] d

# Intrinsic camera matrix for the raw (distorted) images, row-major.
float64[9] k

# Rectification matrix (stereo cameras only), row-major.
float64[9] r

# Projection/camera matrix, row-major.
float64[12] p

# Binning refers here to any camera setting which combines rectangular
# neighborhoods of pixels into larger "super-pixels."
uint32 binning_x
uint32 binning_y

# Region of interest actually used for grabbing the image.
RegionOfInterest roi
