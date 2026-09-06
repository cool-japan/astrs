# Navigation Satellite fix for any Global Navigation Satellite System.

std_msgs/Header header

NavSatStatus status

float64 latitude
float64 longitude
float64 altitude

float64[9] position_covariance

uint8 COVARIANCE_TYPE_UNKNOWN=0
uint8 COVARIANCE_TYPE_APPROXIMATED=1
uint8 COVARIANCE_TYPE_DIAGONAL_KNOWN=2
uint8 COVARIANCE_TYPE_KNOWN=3

uint8 position_covariance_type
