//! [`Geometry`] — the four shapes a `<visual>`/`<collision>`'s
//! `<geometry>` element can hold.

use crate::math::Vec3;

/// A `<geometry>` element's single child: exactly one of URDF's four shape
/// kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum Geometry {
    /// `<box size="x y z"/>` — an axis-aligned box, `size` full (not
    /// half-) extents along the local X/Y/Z axes.
    Box {
        /// The box's full extent along each local axis.
        size: Vec3,
    },
    /// `<cylinder radius="r" length="l"/>` — a cylinder whose axis is the
    /// local Z axis, centered on the local origin.
    Cylinder {
        /// The cylinder's radius.
        radius: f64,
        /// The cylinder's length along Z.
        length: f64,
    },
    /// `<sphere radius="r"/>` — a sphere centered on the local origin.
    Sphere {
        /// The sphere's radius.
        radius: f64,
    },
    /// `<mesh filename="..." scale="x y z"/>` — an external mesh file, not
    /// loaded or interpreted by this crate (blueprint §5.3 asks for a
    /// mesh *path*, not a mesh loader — a `.dae`/`.stl` importer belongs to
    /// a rendering or collision-geometry crate, not the model layer).
    Mesh {
        /// The mesh file path, exactly as written in the URDF (typically a
        /// `package://` URL or a path relative to the URDF file — resolving
        /// either is a caller concern this crate does not perform).
        filename: String,
        /// The per-axis scale factor applied to the mesh, `1 1 1` if the
        /// URDF omitted `scale`.
        scale: Vec3,
    },
}

impl Geometry {
    /// This geometry's kind as URDF's own element name (`"box"`,
    /// `"cylinder"`, `"sphere"`, or `"mesh"`) — for diagnostics, not parsed
    /// from anywhere (the element name is what dispatches parsing in the
    /// first place).
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Box { .. } => "box",
            Self::Cylinder { .. } => "cylinder",
            Self::Sphere { .. } => "sphere",
            Self::Mesh { .. } => "mesh",
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn kind_name_matches_the_urdf_element_name() {
        assert_eq!(Geometry::Box { size: Vec3::ZERO }.kind_name(), "box");
        assert_eq!(
            Geometry::Cylinder {
                radius: 1.0,
                length: 1.0
            }
            .kind_name(),
            "cylinder"
        );
        assert_eq!(Geometry::Sphere { radius: 1.0 }.kind_name(), "sphere");
        assert_eq!(
            Geometry::Mesh {
                filename: "a.stl".to_owned(),
                scale: Vec3::new(1.0, 1.0, 1.0)
            }
            .kind_name(),
            "mesh"
        );
    }
}
