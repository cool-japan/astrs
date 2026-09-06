# astrs-idl

ROS 2 `.msg`, `.srv` and `.action` parsing with Rust code generation for
AstRS.

A hand-rolled recursive-descent front end — no parser-combinator dependency
— and a code generator that puts every ROS type into columnar space
mechanically: `.msg`/`.srv`/`.action` parsing (primitives, arrays, bounded
types, `wstring`, constants and defaults), cross-file resolution of type
references plus the `structure_needs_at_least_one_member` synthesis rule,
`package.xml` and ament-tree discovery for locating interface packages, and
Rust codegen (`proc-macro2`/`quote`/`prettyplease`) emitting types that
implement both `astrs_cdr::CdrSerde` and `astrs_data::AstrsMessage`. The
`common_interfaces` set (`std_msgs`, `geometry_msgs`, `sensor_msgs`,
`nav_msgs`, …) ships pre-generated in-tree, checked for reproducibility by
an always-on drift test (`generated_matches_source`, part of the normal
test run — this repository has no hosted CI beyond its publish workflows),
so bridging standard ROS 2 topics needs no `.msg` files on disk at all
— most callers reach this crate through the `generated` module rather than
the parser/codegen path directly.

See the [crate documentation](https://docs.rs/astrs-idl) for the IDL parsing
and codegen design.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
