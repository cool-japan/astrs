//! [`TrajectoryHasher`] — a fast, deterministic fingerprint of a run's
//! poses, letting [`crate::World`]'s reproducibility promise ("identical
//! seed + inputs -> bit-identical trajectory") be checked with one
//! `assert_eq!` instead of a pose-by-pose comparison.
//!
//! # Not a cryptographic hash, and not trying to be
//!
//! This is the [FNV-1a](http://www.isthe.com/chongo/tech/comp/fnv/) 64-bit
//! algorithm (Fowler/Noll/Vo), chosen for exactly two properties this use
//! case needs and nothing more: it is a few integer operations per byte
//! (cheap enough to run every tick without it becoming the simulation's
//! bottleneck), and it is *sensitive* — flipping any single bit anywhere
//! in the absorbed stream changes the final hash, which is what makes
//! [`crate::World::trajectory_hash`] a meaningful regression check rather
//! than a hash that happens to collide on the inputs this crate's own
//! tests exercise. It offers no resistance to a deliberate adversary
//! constructing a colliding trajectory, which is irrelevant here: nothing
//! in this crate treats this hash as a security boundary, only as "did
//! this run produce exactly the same numbers as that run".

/// The FNV-1a 64-bit offset basis.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// The FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// An FNV-1a accumulator over a stream of `f64` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrajectoryHasher(u64);

impl TrajectoryHasher {
    /// A fresh accumulator, at the FNV-1a offset basis.
    #[must_use]
    pub const fn new() -> Self {
        Self(FNV_OFFSET_BASIS)
    }

    /// Folds one `f64`'s raw bit pattern into the accumulator, byte by
    /// byte (little-endian — the choice is arbitrary, since this hash
    /// never needs to be reproduced by anything outside this crate, only
    /// to be *consistent* within one process across two runs; the
    /// important property is that it is a fixed choice, not which one).
    pub fn absorb_f64(&mut self, value: f64) {
        for byte in value.to_bits().to_le_bytes() {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
        }
    }

    /// Folds `pose`'s three components in, in a fixed `(x, y, theta)`
    /// order.
    pub fn absorb_pose(&mut self, pose: crate::kinematics::Pose2D) {
        self.absorb_f64(pose.x);
        self.absorb_f64(pose.y);
        self.absorb_f64(pose.theta);
    }

    /// The accumulated hash so far. Cheap and side-effect-free to call
    /// mid-stream — [`crate::World::trajectory_hash`] calls this after
    /// every tick, not only at the end of a run.
    #[must_use]
    pub const fn finish(self) -> u64 {
        self.0
    }
}

impl Default for TrajectoryHasher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::kinematics::Pose2D;

    #[test]
    fn a_fresh_hasher_starts_at_the_offset_basis() {
        assert_eq!(TrajectoryHasher::new().finish(), FNV_OFFSET_BASIS);
        assert_eq!(TrajectoryHasher::default().finish(), FNV_OFFSET_BASIS);
    }

    #[test]
    fn absorbing_nothing_different_produces_the_same_hash() {
        let mut a = TrajectoryHasher::new();
        let mut b = TrajectoryHasher::new();
        a.absorb_f64(1.5);
        b.absorb_f64(1.5);
        assert_eq!(a.finish(), b.finish());
    }

    #[test]
    fn absorbing_a_different_value_changes_the_hash() {
        let mut a = TrajectoryHasher::new();
        let mut b = TrajectoryHasher::new();
        a.absorb_f64(1.5);
        b.absorb_f64(1.5000001);
        assert_ne!(a.finish(), b.finish());
    }

    /// The single-bit-flip sensitivity this module's own docs claim: the
    /// smallest possible `f64` perturbation (one ULP) still changes the
    /// hash.
    #[test]
    fn a_single_ulp_difference_changes_the_hash() {
        let mut a = TrajectoryHasher::new();
        let mut b = TrajectoryHasher::new();
        let value = 3.0_f64;
        let next_up = f64::from_bits(value.to_bits() + 1);
        assert_ne!(value, next_up);
        a.absorb_f64(value);
        b.absorb_f64(next_up);
        assert_ne!(a.finish(), b.finish());
    }

    #[test]
    fn order_of_absorption_matters() {
        let mut a = TrajectoryHasher::new();
        a.absorb_f64(1.0);
        a.absorb_f64(2.0);
        let mut b = TrajectoryHasher::new();
        b.absorb_f64(2.0);
        b.absorb_f64(1.0);
        assert_ne!(a.finish(), b.finish());
    }

    #[test]
    fn absorb_pose_folds_all_three_components_in_a_fixed_order() {
        let mut via_pose = TrajectoryHasher::new();
        via_pose.absorb_pose(Pose2D::new(1.0, 2.0, 3.0));

        let mut via_fields = TrajectoryHasher::new();
        via_fields.absorb_f64(1.0);
        via_fields.absorb_f64(2.0);
        via_fields.absorb_f64(3.0);

        assert_eq!(via_pose.finish(), via_fields.finish());
    }

    #[test]
    fn two_poses_differing_only_in_theta_hash_differently() {
        let mut a = TrajectoryHasher::new();
        a.absorb_pose(Pose2D::new(0.0, 0.0, 0.1));
        let mut b = TrajectoryHasher::new();
        b.absorb_pose(Pose2D::new(0.0, 0.0, 0.2));
        assert_ne!(a.finish(), b.finish());
    }
}
