# This is the message to communicate a parameter's descriptor.

# The name of the parameter.
string name

# Enum values are defined in the `ParameterType.msg` message.
uint8 type

# Description of the parameter, visible from introspection tools.
string description

# Plain English description of additional constraints which cannot be expressed
# with the available constraints, e.g. "only prime numbers".
string additional_constraints

# If true then the value cannot change after it has been initialized.
bool read_only false

# If true, the parameter is allowed to change type.
bool dynamic_typing false

# FloatingPointRange and IntegerRange are optional, and at most one may be set.
FloatingPointRange[<=1] floating_point_range
IntegerRange[<=1] integer_range
