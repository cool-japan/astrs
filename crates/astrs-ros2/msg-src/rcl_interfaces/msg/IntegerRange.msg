# Represents bounds and a step value for an integer typed parameter.

# Start value for valid values, inclusive.
int64 from_value

# End value for valid values, inclusive.
int64 to_value

# Size of valid steps between the from and to bound.
#
# A step value of zero implies a continuous range of values.
uint64 step
