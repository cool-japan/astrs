//! A 2-D lidar sensor model: [`LidarConfig`] (rays/FOV/range), and
//! [`cast_scan`], a per-beam grid raycast against a [`crate::grid::Grid`].
//!
//! # The algorithm: Amanatides–Woo fast voxel traversal ("DDA")
//!
//! Each beam is marched cell-by-cell through the occupancy grid rather than
//! sampled at fixed small steps along its length — the "Digital
//! Differential Analyzer" grid-traversal technique (Amanatides & Woo, *A
//! Fast Voxel Traversal Algorithm for Ray Tracing*, 1987). Starting from
//! the cell the beam origin sits in, the algorithm tracks how far (in the
//! beam's own parametric distance `t`) it would have to travel to cross
//! the *next* vertical grid line and the *next* horizontal grid line,
//! always stepping into whichever neighbor that smaller `t` reaches first.
//! This visits every cell the beam actually passes through exactly once,
//! with no risk of a step small enough to leap over a thin wall (a real
//! danger of fixed-step marching) and no wasted work sampling far more
//! points than there are cells to check.
//!
//! # Why lidar reads the world's ground truth, not the odometry estimate
//!
//! [`crate::World::scan`] always rays out from the world's ground-truth
//! pose, never from [`crate::odometry::OdometryModel`]'s (possibly noisy)
//! estimate. A real lidar measures the world the robot is actually
//! standing in; only the robot's own belief about where it stands can
//! drift from that, which is exactly the gap
//! [`crate::odometry::OdometryModel`]'s noise model exists to create — see
//! that module's own docs.

use crate::grid::Grid;
use crate::kinematics::Pose2D;

/// Below this magnitude, a ray direction's component on that axis is
/// treated as exactly zero (the beam never crosses a grid line on that
/// axis at all) rather than as a very small but nonzero slope.
///
/// A beam angle is built from `angle.cos()`/`angle.sin()`, and while
/// `sin(0.0) == 0.0` exactly, the analogous "exactly vertical" case
/// (`cos(FRAC_PI_2)`) is not exactly `0.0` in `f64` — `FRAC_PI_2` is
/// already an approximation of the true irrational `pi/2`, so its cosine
/// carries a residual on the order of `1e-16`. Without this threshold that
/// residual would still produce a *correct* answer (an enormous but finite
/// `t_delta` on that axis, which never ends up the smaller of the two
/// candidates), just by numerical coincidence rather than by design — this
/// crate does not rely on that coincidence. See [`kinematics::ANGULAR_VELOCITY_EPSILON`](crate::kinematics)
/// for the identical reasoning applied to the unicycle integrator's own
/// `w -> 0` branch.
const DIRECTION_EPSILON: f64 = 1e-9;

/// A 2-D lidar's fixed physical configuration: how many beams it casts,
/// over what angular field of view, and the distances it can measure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LidarConfig {
    num_rays: u32,
    angle_min: f64,
    angle_max: f64,
    range_min: f64,
    range_max: f64,
}

impl LidarConfig {
    /// Builds a configuration, or [`None`] if it does not describe a real
    /// sensor: `num_rays` must be at least `1`; `angle_min`/`angle_max`
    /// must be finite with `angle_min <= angle_max`; `range_min`/
    /// `range_max` must be finite with `0.0 <= range_min < range_max`.
    ///
    /// # `angle_min`/`angle_max` are both inclusive endpoints
    ///
    /// Beam `0` sits at exactly `angle_min`; beam `num_rays - 1` sits at
    /// exactly `angle_max` (linearly interpolated in between) — matching
    /// [`astrs_node_api::message::LaserScan`]'s own
    /// `angle_min`/`angle_max`/`angle_increment` convention field for
    /// field. That is exactly right for a genuinely partial field of view
    /// (see the example below: a beam at each boundary of a forward-facing
    /// sensor is meaningful and distinct), but goes wrong the moment
    /// `angle_max - angle_min` is exactly `TAU`: `-π` and `+π` name the
    /// *same* physical direction, so `LidarConfig::new(n, -PI, PI, ..)`
    /// produces `n` beams spaced `TAU / (n - 1)` apart (not the `TAU / n`
    /// a reader would expect) with the first and last physically
    /// redundant. **Use [`LidarConfig::full_circle`] for a spinning
    /// 360° sensor** — it exists specifically to avoid this pitfall,
    /// which this crate's own first draft of `astrs-sim-node`'s default
    /// lidar configuration fell into.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_sim::lidar::LidarConfig;
    ///
    /// // A forward-facing 180-degree automotive-style sensor: a beam at
    /// // each boundary (-90 and +90 degrees) is meaningful, so the
    /// // inclusive-endpoint convention is exactly right here.
    /// let config = LidarConfig::new(181, -std::f64::consts::FRAC_PI_2, std::f64::consts::FRAC_PI_2, 0.1, 12.0).unwrap();
    /// assert!((config.angle_increment() - std::f64::consts::PI / 180.0).abs() < 1e-12);
    ///
    /// assert!(LidarConfig::new(0, 0.0, 1.0, 0.1, 10.0).is_none()); // no beams
    /// assert!(LidarConfig::new(10, 1.0, 0.0, 0.1, 10.0).is_none()); // max < min
    /// assert!(LidarConfig::new(10, 0.0, 1.0, 10.0, 10.0).is_none()); // range_min == range_max
    /// ```
    #[must_use]
    pub fn new(
        num_rays: u32,
        angle_min: f64,
        angle_max: f64,
        range_min: f64,
        range_max: f64,
    ) -> Option<Self> {
        if num_rays == 0 {
            return None;
        }
        if !angle_min.is_finite() || !angle_max.is_finite() || angle_min > angle_max {
            return None;
        }
        if !range_min.is_finite()
            || !range_max.is_finite()
            || range_min < 0.0
            || range_min >= range_max
        {
            return None;
        }
        Some(Self {
            num_rays,
            angle_min,
            angle_max,
            range_min,
            range_max,
        })
    }

    /// Builds a configuration for a full 360° spinning sensor — `num_rays`
    /// beams evenly spaced around the *entire* circle, with **no
    /// duplicated seam beam**.
    ///
    /// This is deliberately not `LidarConfig::new(num_rays, -PI, PI, ..)`:
    /// see [`LidarConfig::new`]'s own docs for exactly why that spacing is
    /// wrong for a full revolution (the endpoints name the same physical
    /// direction). Internally this picks `angle_min = -PI` and an
    /// `angle_max` strictly less than `PI` — specifically `num_rays - 1`
    /// steps of `TAU / num_rays` past `angle_min` — so
    /// [`LidarConfig::angle_increment`] comes out to exactly `TAU /
    /// num_rays` and every beam is a genuinely distinct bearing.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_sim::lidar::LidarConfig;
    /// use std::f64::consts::TAU;
    ///
    /// let config = LidarConfig::full_circle(360, 0.1, 12.0).unwrap();
    /// let step = TAU / 360.0;
    /// assert!((config.angle_increment() - step).abs() < 1e-9);
    /// // Taking one more step past the LAST beam lands exactly one full
    /// // turn past the FIRST beam — 360 beams close a 360-gap circle,
    /// // never colliding on the same bearing along the way.
    /// let first = config.beam_angle(0).unwrap();
    /// let last = config.beam_angle(359).unwrap();
    /// assert!(((last + step) - (first + TAU)).abs() < 1e-6);
    /// ```
    #[must_use]
    pub fn full_circle(num_rays: u32, range_min: f64, range_max: f64) -> Option<Self> {
        if num_rays == 0 {
            return None;
        }
        let angle_min = -std::f64::consts::PI;
        let step = std::f64::consts::TAU / f64::from(num_rays);
        let angle_max = angle_min + step * f64::from(num_rays - 1);
        Self::new(num_rays, angle_min, angle_max, range_min, range_max)
    }

    /// How many beams one sweep casts.
    #[must_use]
    pub const fn num_rays(&self) -> u32 {
        self.num_rays
    }

    /// The first beam's angle, radians, relative to the sensor's own
    /// heading.
    #[must_use]
    pub const fn angle_min(&self) -> f64 {
        self.angle_min
    }

    /// The last beam's angle, radians, relative to the sensor's own
    /// heading.
    #[must_use]
    pub const fn angle_max(&self) -> f64 {
        self.angle_max
    }

    /// The shortest distance this sensor can report, in metres.
    #[must_use]
    pub const fn range_min(&self) -> f64 {
        self.range_min
    }

    /// The longest distance this sensor can report, in metres.
    #[must_use]
    pub const fn range_max(&self) -> f64 {
        self.range_max
    }

    /// The angular step between consecutive beams.
    ///
    /// `0.0` for a single-beam (`num_rays == 1`) configuration, for which
    /// "the step to the next beam" has no meaning — matching
    /// [`astrs_node_api::message::LaserScan::angle_increment`]'s own
    /// implicit convention (a `LaserScan` with one range has no other
    /// sensible value to report there).
    #[must_use]
    pub fn angle_increment(&self) -> f64 {
        if self.num_rays <= 1 {
            0.0
        } else {
            (self.angle_max - self.angle_min) / f64::from(self.num_rays - 1)
        }
    }

    /// The sensor-relative angle of beam `index`, or [`None`] if `index >=
    /// num_rays`.
    #[must_use]
    pub fn beam_angle(&self, index: u32) -> Option<f64> {
        if index >= self.num_rays {
            return None;
        }
        Some(self.angle_min + self.angle_increment() * f64::from(index))
    }
}

/// One sweep's worth of range readings.
#[derive(Debug, Clone, PartialEq)]
pub struct LidarScan {
    /// The configuration this scan was cast with.
    pub config: LidarConfig,
    /// One range per beam, in beam order — `config.num_rays()` entries,
    /// each in metres. [`f32::INFINITY`] means "no return": no occupied
    /// cell lay within `config.range_max()` along that beam, matching REP
    /// 117's convention for a 2-D lidar's "out of range" reading. A hit is
    /// reported at its exact geometric distance even when that is below
    /// `config.range_min()` — `range_min`/`range_max` describe the
    /// sensor's own spec, not a clamp this simulator imposes on an
    /// otherwise-exact synthetic measurement.
    pub ranges: Vec<f32>,
}

/// Casts a full sweep from `pose` (the beam origin and the frame
/// `config`'s angles are relative to) against `grid`.
///
/// # Examples
///
/// ```
/// use astrs_sim::grid::Grid;
/// use astrs_sim::kinematics::Pose2D;
/// use astrs_sim::lidar::{LidarConfig, cast_scan};
///
/// // A 1-wide corridor, walls at x=0 and x=9, robot centered at x=4.5.
/// let grid = Grid::from_text("10 1 1.0\n#........#\n").unwrap();
/// let pose = Pose2D::new(4.5, 0.5, 0.0); // facing +X
/// let config = LidarConfig::new(1, 0.0, 0.0, 0.0, 20.0).unwrap();
/// let scan = cast_scan(pose, &grid, &config);
/// assert!((scan.ranges[0] - 4.5).abs() < 1e-4); // wall at x=9's near edge
/// ```
#[must_use]
pub fn cast_scan(pose: Pose2D, grid: &Grid, config: &LidarConfig) -> LidarScan {
    let ranges = (0..config.num_rays())
        .map(|index| {
            // `LidarConfig::beam_angle` only returns `None` for an
            // out-of-range index, which this loop never produces.
            let local_angle = config.beam_angle(index).unwrap_or(config.angle_min());
            cast_ray(
                pose.x,
                pose.y,
                pose.theta + local_angle,
                grid,
                config.range_max(),
            )
        })
        .collect();
    LidarScan {
        config: *config,
        ranges,
    }
}

/// The `(step, t_delta, t_max)` triple [`cast_ray`]'s DDA loop needs for
/// one axis: how the integer cell index changes per crossing (`step`), how
/// much `t` advances per full cell crossed (`t_delta`), and the `t` at
/// which the *first* crossing on this axis happens (`t_max`).
///
/// `component` is the ray direction's component on this axis (`dx` or
/// `dy`); `origin_grid` is the ray origin's continuous grid coordinate on
/// this axis; `cell` is the integer cell index (`floor(origin_grid)`) the
/// origin starts in.
fn axis_step(component: f64, origin_grid: f64, cell: i64) -> (i64, f64, f64) {
    if component.abs() < DIRECTION_EPSILON {
        // The ray never crosses a grid line on this axis: `t_delta`/
        // `t_max` of `+inf` mean this axis is never the smaller candidate
        // in the traversal loop below, so `step` (never applied) can be
        // any value — `0` documents "this axis does not move".
        return (0, f64::INFINITY, f64::INFINITY);
    }
    let t_delta = 1.0 / component.abs();
    if component > 0.0 {
        #[expect(
            clippy::cast_precision_loss,
            reason = "cell indices are bounded by grid dimensions, far below f64's exact-integer ceiling"
        )]
        let next_boundary = (cell + 1) as f64;
        (1, t_delta, (next_boundary - origin_grid) / component)
    } else {
        #[expect(
            clippy::cast_precision_loss,
            reason = "cell indices are bounded by grid dimensions, far below f64's exact-integer ceiling"
        )]
        let this_boundary = cell as f64;
        (-1, t_delta, (this_boundary - origin_grid) / component)
    }
}

/// Marches one beam from `(origin_x, origin_y)` at world-frame angle
/// `heading` through `grid`'s cells (Amanatides–Woo traversal, see [module
/// docs](self)), returning the distance to the first occupied cell's near
/// edge, or [`f32::INFINITY`] if none lies within `range_max`.
fn cast_ray(origin_x: f64, origin_y: f64, heading: f64, grid: &Grid, range_max: f64) -> f32 {
    let direction = (heading.cos(), heading.sin());
    let resolution = grid.resolution();
    let origin_grid = grid.world_to_grid(origin_x, origin_y);

    let mut cell = (origin_grid.0.floor() as i64, origin_grid.1.floor() as i64);
    match grid.cell(cell.0, cell.1) {
        // The beam origin itself is outside the mapped area: no return.
        None => return f32::INFINITY,
        // The sensor is (already) embedded in an occupied cell.
        Some(true) => return 0.0,
        Some(false) => {}
    }

    let (step_x, t_delta_x, mut t_max_x) = axis_step(direction.0, origin_grid.0, cell.0);
    let (step_y, t_delta_y, mut t_max_y) = axis_step(direction.1, origin_grid.1, cell.1);
    let t_limit = range_max / resolution;

    loop {
        let t = if t_max_x <= t_max_y {
            cell.0 += step_x;
            let t = t_max_x;
            t_max_x += t_delta_x;
            t
        } else {
            cell.1 += step_y;
            let t = t_max_y;
            t_max_y += t_delta_y;
            t
        };
        if t > t_limit {
            return f32::INFINITY;
        }
        match grid.cell(cell.0, cell.1) {
            None => return f32::INFINITY,
            Some(true) => {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "a range reading is reported at f32 precision, std/sensor/v1/LaserScan's own wire contract"
                )]
                return (t * resolution) as f32;
            }
            Some(false) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    // ---- LidarConfig ------------------------------------------------------

    #[test]
    fn angle_increment_divides_the_fov_across_the_gaps_between_beams() {
        let config = LidarConfig::new(5, 0.0, std::f64::consts::PI, 0.1, 10.0).unwrap();
        // 5 beams -> 4 gaps across pi radians.
        assert!((config.angle_increment() - std::f64::consts::PI / 4.0).abs() < 1e-12);
        assert!((config.beam_angle(0).unwrap() - 0.0).abs() < 1e-12);
        assert!((config.beam_angle(4).unwrap() - std::f64::consts::PI).abs() < 1e-12);
        assert_eq!(config.beam_angle(5), None);
    }

    #[test]
    fn a_single_beam_configuration_has_a_zero_increment() {
        let config = LidarConfig::new(1, 0.5, 0.5, 0.1, 10.0).unwrap();
        assert_eq!(config.angle_increment(), 0.0);
        assert_eq!(config.beam_angle(0), Some(0.5));
    }

    /// The bug this module's own docs describe: a naive full-circle
    /// `LidarConfig::new(n, -PI, PI, ..)` spaces its `n` beams `TAU /
    /// (n - 1)` apart, one fewer gap than it should have — `n` beams
    /// around a circle have `n` gaps, not `n - 1`, because the circle
    /// wraps. This is exactly what `LidarConfig::full_circle` exists to
    /// avoid.
    #[test]
    fn a_naive_full_circle_via_new_has_the_wrong_spacing() {
        let naive =
            LidarConfig::new(36, -std::f64::consts::PI, std::f64::consts::PI, 0.1, 10.0).unwrap();
        let wrong_increment = std::f64::consts::TAU / 35.0; // n - 1 gaps
        let right_increment = std::f64::consts::TAU / 36.0; // n gaps
        assert!((naive.angle_increment() - wrong_increment).abs() < 1e-9);
        assert!((naive.angle_increment() - right_increment).abs() > 1e-4);
    }

    #[test]
    fn full_circle_spaces_every_beam_evenly_with_no_duplicated_seam() {
        let config = LidarConfig::full_circle(36, 0.1, 10.0).unwrap();
        let expected_increment = std::f64::consts::TAU / 36.0;
        assert!((config.angle_increment() - expected_increment).abs() < 1e-9);
        assert_eq!(config.num_rays(), 36);

        // The last beam plus one more step of the increment lands exactly
        // one full turn past the first beam — proving no beam was "used
        // up" on a duplicate seam.
        let first = config.beam_angle(0).unwrap();
        let last = config.beam_angle(35).unwrap();
        assert!((last + config.angle_increment() - first - std::f64::consts::TAU).abs() < 1e-6);
    }

    #[test]
    fn full_circle_rejects_zero_rays_but_accepts_one() {
        assert!(LidarConfig::full_circle(0, 0.1, 10.0).is_none());
        let single = LidarConfig::full_circle(1, 0.1, 10.0).unwrap();
        assert_eq!(single.num_rays(), 1);
        assert_eq!(single.angle_increment(), 0.0);
    }

    #[test]
    fn invalid_configurations_are_rejected() {
        assert!(LidarConfig::new(0, 0.0, 1.0, 0.1, 10.0).is_none());
        assert!(LidarConfig::new(4, 1.0, 0.0, 0.1, 10.0).is_none());
        assert!(LidarConfig::new(4, 0.0, 1.0, -0.1, 10.0).is_none());
        assert!(LidarConfig::new(4, 0.0, 1.0, 10.0, 10.0).is_none());
        assert!(LidarConfig::new(4, 0.0, 1.0, 11.0, 10.0).is_none());
        assert!(LidarConfig::new(4, f64::NAN, 1.0, 0.1, 10.0).is_none());
        assert!(LidarConfig::new(4, 0.0, 1.0, 0.1, f64::INFINITY).is_none());
    }

    // ---- Golden raycast cases ----------------------------------------------

    /// A single vertical wall column, hit by a due-east beam: the exact
    /// distance to the wall's near edge, hand-computed.
    #[test]
    fn a_due_east_beam_hits_a_wall_at_its_exact_near_edge() {
        // 10x10, resolution 1.0, occupied column at gx=5 (x in [5, 6)).
        let mut rows = Vec::new();
        for _ in 0..10 {
            let mut row = String::new();
            for x in 0..10 {
                row.push(if x == 5 { '#' } else { '.' });
            }
            rows.push(row);
        }
        let text = format!("10 10 1.0\n{}\n", rows.join("\n"));
        let grid = Grid::from_text(&text).unwrap();

        // Sensor at the center of cell (2, 2): x=2.5.
        let distance = cast_ray(2.5, 2.5, 0.0, &grid, 20.0);
        assert!((distance - 2.5).abs() < 1e-4, "distance was {distance}");
    }

    /// Same wall, approached from due north (a `dx` near, but not exactly,
    /// zero — see [`DIRECTION_EPSILON`]'s own docs) to prove the "no
    /// crossing on this axis" branch does not derail a beam that only
    /// approximately aligns with an axis. The beam here travels parallel
    /// to the wall and must never hit it.
    #[test]
    fn a_due_north_beam_parallel_to_the_wall_never_hits_it() {
        let mut rows = Vec::new();
        for _ in 0..10 {
            let mut row = String::new();
            for x in 0..10 {
                row.push(if x == 5 { '#' } else { '.' });
            }
            rows.push(row);
        }
        let text = format!("10 10 1.0\n{}\n", rows.join("\n"));
        let grid = Grid::from_text(&text).unwrap();

        let distance = cast_ray(2.5, 2.5, std::f64::consts::FRAC_PI_2, &grid, 20.0);
        assert_eq!(distance, f32::INFINITY);
    }

    /// A beam at a non-axis-aligned, non-45-degree angle (a 3-4-5 slope,
    /// chosen so the hit point is not also an exact grid-line crossing on
    /// the other axis) hits the wall's near edge at a hand-computed
    /// Euclidean distance.
    #[test]
    fn a_diagonal_beam_hits_the_wall_at_the_hand_computed_distance() {
        let mut rows = Vec::new();
        for _ in 0..10 {
            let mut row = String::new();
            for x in 0..10 {
                row.push(if x == 5 { '#' } else { '.' });
            }
            rows.push(row);
        }
        let text = format!("10 10 1.0\n{}\n", rows.join("\n"));
        let grid = Grid::from_text(&text).unwrap();

        // direction (0.8, 0.6): a unit vector, so t itself is the traveled
        // distance. x(t) = 0.5 + 0.8t hits the wall's near edge (x=5.0) at
        // t = 4.5 / 0.8 = 5.625; y(t) = 0.5 + 0.6*5.625 = 3.875, well
        // inside the grid and not itself a y grid-line crossing.
        let heading = 0.6_f64.atan2(0.8);
        let distance = cast_ray(0.5, 0.5, heading, &grid, 20.0);
        assert!((distance - 5.625).abs() < 1e-4, "distance was {distance}");
    }

    #[test]
    fn a_ray_with_nothing_in_range_reports_no_return() {
        let grid = Grid::from_text("10 10 1.0\n..........\n..........\n..........\n..........\n..........\n..........\n..........\n..........\n..........\n..........\n").unwrap();
        let distance = cast_ray(0.5, 0.5, 0.0, &grid, 5.0);
        assert_eq!(distance, f32::INFINITY);
    }

    #[test]
    fn a_ray_that_exits_the_grid_bounds_before_max_range_reports_no_return() {
        // Fully free 3x3 grid; range_max is generous, so this exercises
        // the "walked off the edge of the map" path, not "ran past
        // range_max" — the two distinct no-hit paths in `cast_ray`.
        let grid = Grid::from_text("3 3 1.0\n...\n...\n...\n").unwrap();
        let distance = cast_ray(1.5, 1.5, 0.0, &grid, 1000.0);
        assert_eq!(distance, f32::INFINITY);
    }

    #[test]
    fn a_sensor_embedded_in_a_wall_reports_zero_range() {
        let grid = Grid::from_text("3 1 1.0\n.#.\n").unwrap();
        let distance = cast_ray(1.5, 0.5, 0.0, &grid, 10.0);
        assert_eq!(distance, 0.0);
    }

    #[test]
    fn a_sensor_outside_the_mapped_area_reports_no_return() {
        let grid = Grid::from_text("3 3 1.0\n...\n...\n...\n").unwrap();
        let distance = cast_ray(-5.0, -5.0, 0.0, &grid, 10.0);
        assert_eq!(distance, f32::INFINITY);
    }

    #[test]
    fn a_wall_beyond_range_max_is_not_reported() {
        // Same wall-column layout as the due-east golden case, but with a
        // range_max shorter than the true 2.5m distance to the wall.
        let mut rows = Vec::new();
        for _ in 0..10 {
            let mut row = String::new();
            for x in 0..10 {
                row.push(if x == 5 { '#' } else { '.' });
            }
            rows.push(row);
        }
        let text = format!("10 10 1.0\n{}\n", rows.join("\n"));
        let grid = Grid::from_text(&text).unwrap();
        let distance = cast_ray(2.5, 2.5, 0.0, &grid, 2.0);
        assert_eq!(distance, f32::INFINITY);
    }

    /// A robot centered in a square room sees all four walls at the exact
    /// same, hand-known distance along the four cardinal beams.
    #[test]
    fn cast_scan_sees_all_four_walls_of_a_square_room_at_equal_known_distances() {
        // An 11x11 room (indices 0..=10), walls on the border, free
        // interior — robot dead-center at cell (5,5) -> world (5.5, 5.5).
        let mut rows = Vec::new();
        for y in 0..11 {
            let mut row = String::new();
            for x in 0..11 {
                let wall = x == 0 || x == 10 || y == 0 || y == 10;
                row.push(if wall { '#' } else { '.' });
            }
            rows.push(row);
        }
        let text = format!("11 11 1.0\n{}\n", rows.join("\n"));
        let grid = Grid::from_text(&text).unwrap();

        let pose = Pose2D::new(5.5, 5.5, 0.0);
        // Four beams: east, north, west, south (angle_min=0, step=pi/2).
        let config =
            LidarConfig::new(4, 0.0, 3.0 * std::f64::consts::FRAC_PI_2, 0.05, 20.0).unwrap();
        let scan = cast_scan(pose, &grid, &config);

        // The near edge of each border wall is 4.5m from the center.
        for (index, range) in scan.ranges.iter().enumerate() {
            assert!(
                (range - 4.5).abs() < 1e-3,
                "beam {index}: {range} (expected ~4.5)"
            );
        }
    }

    #[test]
    fn cast_scan_produces_exactly_num_rays_readings() {
        let grid = Grid::from_text("3 3 1.0\n...\n...\n...\n").unwrap();
        let config = LidarConfig::new(37, -1.0, 1.0, 0.1, 10.0).unwrap();
        let scan = cast_scan(Pose2D::ORIGIN, &grid, &config);
        assert_eq!(scan.ranges.len(), 37);
        assert_eq!(scan.config, config);
    }
}
