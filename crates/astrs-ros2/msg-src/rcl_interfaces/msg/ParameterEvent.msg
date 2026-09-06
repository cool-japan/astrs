# This message contains a parameter event.
# Because the parameter event was an atomic update, a specific parameter name
# can only be in one of the three sets.

# Time stamp of the event.
builtin_interfaces/Time stamp

# Fully qualified ROS path to node.
string node

# The parameters that have been newly declared.
Parameter[] new_parameters

# The parameters that have changed value.
Parameter[] changed_parameters

# The parameters that have been undeclared.
Parameter[] deleted_parameters
