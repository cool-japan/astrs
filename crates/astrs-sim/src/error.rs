//! [`SimError`] — the crate-wide error taxonomy for the fallible corners of
//! this crate that are not already fully described by a `bool`/[`Option`]
//! at their own call site (see, e.g., [`crate::kinematics::DiffDriveGeometry::new`]
//! or [`crate::lidar::LidarConfig::new`], which reject a malformed
//! construction with [`None`] rather than a typed error, matching
//! `astrs_urdf`'s own house style for "this input just does not describe a
//! real geometry/sensor" versus "this operation failed for a reason worth
//! naming").
//!
//! [`SimError`] exists specifically for [`crate::arm::ArmState`], the one
//! place this crate's own logic sits *between* two other crates' fallible
//! APIs — [`astrs_urdf`]'s forward kinematics and [`astrs_tf`]'s transform
//! buffer — and needs one error type that can carry either.

/// The result type this crate's cross-cutting fallible operations return.
pub type Result<T> = std::result::Result<T, SimError>;

/// A simulation failure that crosses this crate's own module boundaries.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SimError {
    /// A [`crate::grid::Grid`] failed to parse.
    #[error(transparent)]
    Grid(#[from] crate::grid::GridError),
    /// [`astrs_urdf`] parsing, validation, or forward-kinematics failure —
    /// see [`crate::arm::ArmState`].
    #[error(transparent)]
    Urdf(#[from] astrs_urdf::UrdfError),
    /// An [`astrs_tf::buffer::TransformBuffer`] insertion failure — see
    /// [`crate::arm::ArmState::populate_transform_buffer`].
    #[error(transparent)]
    Tf(#[from] astrs_tf::TfError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn a_grid_error_converts_via_from_and_displays_its_inner_message() {
        let grid_error = crate::grid::GridError::MissingHeader;
        let sim_error: SimError = grid_error.clone().into();
        assert!(matches!(sim_error, SimError::Grid(_)));
        assert_eq!(sim_error.to_string(), grid_error.to_string());
    }

    #[test]
    fn an_urdf_error_converts_via_from() {
        let urdf_error = astrs_urdf::UrdfError::UnknownLink {
            link: "nope".to_owned(),
        };
        let sim_error: SimError = urdf_error.into();
        assert!(matches!(sim_error, SimError::Urdf(_)));
    }
}
