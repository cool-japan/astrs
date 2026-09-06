# astrs-urdf

URDF robot models for AstRS: links, joints and kinematic chains feeding
`astrs-tf`.

`astrs-tf` deliberately implements the tf2 *contract* — the frame tree, its
SE(3) math and its wire interop — and stops there. This crate is the piece
its documentation points at:

- **`xml`** — a minimal, position-tracked XML 1.0 pull parser, this crate's
  own, with no dependency beyond `std`: elements, attributes, text, CDATA,
  comments, the five predefined entities plus numeric character
  references, UTF-8, depth/size limits, and precise `line:column` error
  spans.
- **`model`** — the URDF object model (`Robot`/`Link`/`Joint` and every
  nested element URDF defines: inertial, visual, collision, geometry,
  material, joint limits/dynamics/mimic/safety-controller) plus
  `Robot::validate`'s tree-shape checks (unique names, every reference
  resolves, the joint graph is a rooted tree, mimic relationships resolve
  and don't cycle).
- **`parse`** — builds a `Robot` from URDF XML text on top of `xml`.
- **`math`** — a small, dependency-free SE(3) implementation
  (`Vec3`/`Quat`/`Transform`) consistent with `astrs_tf::math`'s own
  conventions, used internally by forward kinematics and converted to
  `astrs_tf`'s types only at the boundary.
- **`kinematics`** — chain extraction, forward kinematics (a joint-position
  map to per-link transforms, including `<mimic>` resolution), and
  conversion of the fixed-frame skeleton into `astrs_tf::buffer::TransformBuffer`
  static transforms.

## Example

```rust
use astrs_urdf::JointKind;

// Only the bounded joint types carry `<limit lower=.. upper=..>`.
assert!(JointKind::Revolute.has_position_limits());
assert!(JointKind::Prismatic.has_position_limits());

// A continuous joint spins forever; a fixed joint never moves at all.
assert!(!JointKind::Continuous.has_position_limits());
assert_eq!(JointKind::Fixed.degrees_of_freedom(), 0);
assert_eq!(JointKind::Floating.degrees_of_freedom(), 6);
```

See `examples/parse_and_inspect.rs` for a full parse -> validate -> extract
a chain -> run forward kinematics walkthrough, and `tests/three_dof_arm.rs`/
`tests/diff_drive_base.rs` for hand-derived, independently-cross-checked
golden forward-kinematics fixtures.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
