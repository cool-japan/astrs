//! A minimal walkthrough of this crate's library API: build a
//! [`astrs_sim::World`], tick it under a constant command, and inspect
//! both its ground-truth pose and a lidar sweep — the same shape
//! `astrs-urdf`'s own `examples/parse_and_inspect.rs` uses for that
//! crate's walkthrough.
//!
//! This is the library-level counterpart to `dataflow.yml` (this crate's
//! own root): that manifest drives the same `World` through the
//! `astrs-sim-node`/`astrs-sim-teleop`/`astrs-sim-logger` binaries inside
//! a real AstRS graph; this example drives it directly, with no daemon,
//! no dataflow, and no wire encoding at all — exactly the layering
//! `astrs_sim::wire`'s own docs describe (every other module works in
//! plain `f64`/`Pose2D`/`Velocity2D` terms; only the node binaries pay
//! the wire-conversion cost).
//!
//! ```text
//! cargo run -p astrs-sim --example quickstart
//! ```

use astrs_sim::grid::Grid;
use astrs_sim::kinematics::{DiffDriveGeometry, Pose2D, Velocity2D};
use astrs_sim::lidar::LidarConfig;
use astrs_sim::{Timestep, World};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A 5x5 room, 1m cells: a border wall around a 3x3 free interior.
    let grid = Grid::from_text("5 5 1.0\n#####\n#...#\n#...#\n#...#\n#####\n")?;

    // A 0.3m-wide base on 0.05m-radius wheels.
    let geometry = DiffDriveGeometry::new(0.3, 0.05)
        .ok_or("wheel base/radius must both be finite and positive")?;

    // An 8-beam, full-circle lidar, 0.05..10m range. `full_circle`, not
    // `LidarConfig::new(8, 0.0, TAU, ..)` — the latter would put a beam at
    // both 0 and TAU radians, the same physical bearing, wasting one beam
    // on a duplicate; see `LidarConfig::new`'s own docs.
    let lidar = LidarConfig::full_circle(8, 0.05, 10.0)
        .ok_or("lidar configuration must describe a real sensor")?;

    // 20 Hz: a 50ms fixed step.
    let dt = Timestep::from_hz(20).ok_or("20 Hz must divide one second exactly")?;

    // Seed 42, no odometry noise (sigma = 0.0): ground truth and the
    // odometry estimate will stay bit-identical for this whole run.
    let mut world = World::new(
        grid,
        geometry,
        lidar,
        dt,
        42,
        0.0,
        Pose2D::new(2.5, 2.5, 0.0),
    )
    .ok_or("a zero noise sigma is always accepted")?;

    // Drive in a gentle arc for 40 ticks (2 simulated seconds at 20 Hz).
    let command = Velocity2D::new(0.2, 0.3);
    for _ in 0..40 {
        world.tick(command);
    }

    println!("ticks simulated: {}", world.tick_count());
    println!("ground truth pose: {:?}", world.ground_truth_pose());
    println!("odometry estimate: {:?}", world.odometry_pose());

    let scan = world.scan();
    println!("lidar sweep: {} beams", scan.ranges.len());
    for (index, range) in scan.ranges.iter().enumerate() {
        if range.is_finite() {
            println!("  beam {index}: {range:.3} m");
        } else {
            println!("  beam {index}: no return");
        }
    }

    println!("trajectory hash: {:#x}", world.trajectory_hash());
    Ok(())
}
