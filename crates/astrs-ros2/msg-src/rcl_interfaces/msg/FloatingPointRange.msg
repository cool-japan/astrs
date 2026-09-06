# Represents bounds and a step value for a floating point typed parameter.

# Start value for valid values, inclusive.
float64 from_value

# End value for valid values, inclusive.
float64 to_value

# Size of valid steps between the from and to bound.
#
# Step is considered to be a magnitude, therefore negative values are treated
# the same as positive values, and a step value of zero implies a continuous
# range of values.
float64 step
