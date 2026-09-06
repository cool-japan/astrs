//! Parses a URDF document, validates it, extracts a kinematic chain, and
//! runs forward kinematics at a couple of joint configurations — the same
//! `three_dof_arm` fixture `tests/three_dof_arm.rs`'s golden-value tests
//! check byte-for-byte, so this example's printed output is itself a
//! worked, hand-verifiable trace of exactly what this crate computes.
//!
//! Run it with:
//!
//! ```text
//! cargo run -p astrs-urdf --example parse_and_inspect
//! ```

use std::collections::HashMap;
use std::f64::consts::FRAC_PI_2;

use astrs_urdf::kinematics::{Chain, JointPosition, Topology, forward_kinematics};

/// The three-link arm this example walks: `base_link` -> (revolute, axis
/// Z) -> `link1` -> (revolute, axis Y) -> `link2` -> (prismatic, axis X)
/// -> `link3`, with every joint origin a unit translation along a world
/// axis (see `tests/fixtures/three_dof_arm.urdf`'s own comment for the
/// full layout this text draws).
const URDF: &str = r#"<?xml version="1.0"?>
<robot name="three_dof_arm">
  <link name="base_link"/>
  <link name="link1"/>
  <link name="link2"/>
  <link name="link3"/>

  <joint name="j1" type="revolute">
    <parent link="base_link"/>
    <child link="link1"/>
    <axis xyz="0 0 1"/>
    <limit lower="-3.14159265" upper="3.14159265" velocity="2.0" effort="20.0"/>
  </joint>

  <joint name="j2" type="revolute">
    <origin xyz="1 0 0"/>
    <parent link="link1"/>
    <child link="link2"/>
    <axis xyz="0 1 0"/>
    <limit lower="-3.14159265" upper="3.14159265" velocity="2.0" effort="15.0"/>
  </joint>

  <joint name="j3" type="prismatic">
    <origin xyz="1 0 0"/>
    <parent link="link2"/>
    <child link="link3"/>
    <axis xyz="1 0 0"/>
    <limit lower="0.0" upper="1.0" velocity="0.5" effort="10.0"/>
  </joint>
</robot>
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Parse. `parse_str` checks XML well-formedness and URDF element
    //    shape, but not cross-references or tree shape — that is
    //    `Robot::validate`'s separate job (see its own docs on why).
    let robot = astrs_urdf::parse_str(URDF)?;
    println!(
        "parsed robot '{}': {} links, {} joints",
        robot.name,
        robot.links.len(),
        robot.joints.len()
    );

    // 2. Validate: unique names, every parent/child link exists, and the
    //    joint graph is a single rooted tree.
    robot.validate()?;
    println!("validated: a well-formed rooted tree\n");

    // 3. Topology: the root link, and every link's parent joint.
    let topology = Topology::build(&robot)?;
    println!("root link: {}", topology.root());
    for link in &robot.links {
        match topology.parent_joint(&link.name)? {
            Some(joint) => println!("  {} <- {} ({})", link.name, joint.name, joint.kind),
            None => println!("  {} (root)", link.name),
        }
    }
    println!();

    // 4. Extract the full base-to-tip chain and a partial one.
    let full_chain = Chain::extract(&robot, "base_link", "link3")?;
    println!(
        "chain base_link -> link3: {:?} ({} DOF)",
        full_chain.joint_names(),
        full_chain.degrees_of_freedom(&robot)
    );
    let partial_chain = Chain::extract(&robot, "link1", "link3")?;
    println!(
        "chain link1 -> link3: {:?} ({} DOF)\n",
        partial_chain.joint_names(),
        partial_chain.degrees_of_freedom(&robot)
    );

    // 5. Forward kinematics at the home position: every joint at zero.
    let home = forward_kinematics(&robot, &HashMap::new())?;
    println!("home position (every joint at zero):");
    for link in &robot.links {
        let t = &home[&link.name];
        println!(
            "  {}: translation=({:.3}, {:.3}, {:.3})",
            link.name, t.translation.x, t.translation.y, t.translation.z
        );
    }
    println!();

    // 6. Forward kinematics with j1 rotated a quarter turn about Z: the
    //    whole arm swings from the +X axis onto the +Y axis.
    let mut positions = HashMap::new();
    positions.insert("j1".to_owned(), JointPosition::Scalar(FRAC_PI_2));
    let rotated = forward_kinematics(&robot, &positions)?;
    println!("j1 = pi/2 (arm swung onto +Y):");
    for link in &robot.links {
        let t = &rotated[&link.name];
        println!(
            "  {}: translation=({:.3}, {:.3}, {:.3})",
            link.name, t.translation.x, t.translation.y, t.translation.z
        );
    }

    Ok(())
}
