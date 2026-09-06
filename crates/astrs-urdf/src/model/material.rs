//! [`Material`] and [`Color`] — a `<link><visual><material>` element, or a
//! robot-level `<robot><material>` declaration it can reference by name.

/// An RGBA color, each component `0.0..=1.0` by URDF convention (not
/// enforced by this type — see [`crate::model::Robot::validate`] for what
/// *is* checked).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    /// Red.
    pub r: f64,
    /// Green.
    pub g: f64,
    /// Blue.
    pub b: f64,
    /// Alpha (opacity).
    pub a: f64,
}

impl Color {
    /// Opaque white — URDF's own default when a `<material>` gives neither
    /// a `<color>` nor a `<texture>` (a bare `<material name=".../>` with
    /// no children, referencing a robot-level material by name instead).
    pub const WHITE: Self = Self::new(1.0, 1.0, 1.0, 1.0);

    /// Builds a color from its four components.
    #[must_use]
    pub const fn new(r: f64, g: f64, b: f64, a: f64) -> Self {
        Self { r, g, b, a }
    }
}

impl Default for Color {
    fn default() -> Self {
        Self::WHITE
    }
}

/// A `<material>` element: either declared once at `<robot>` level and
/// referenced by name from any number of `<visual>`s, or declared inline
/// inside one `<visual>`.
///
/// URDF allows a bare `<material name="foo"/>` inside a `<visual>` with no
/// `<color>`/`<texture>` children at all, meaning "use the robot-level
/// material named `foo`" — such a `<visual>` still parses to
/// [`super::Visual::material`] holding [`Material::named`]'s exact shape
/// (`color: None, texture_filename: None`), rather than being eagerly
/// resolved against `<robot>`-level `<material>` declarations at parse
/// time: those declarations can legally appear *after* the `<link>` that
/// references them in document order (this crate's pull parser sees the
/// document exactly once, so a reference cannot look ahead), and a real
/// URDF toolchain's own convention is to resolve the reference by name once
/// the whole document — robot-level materials included — is available.
/// [`super::Robot::resolve_material`] is that resolution step, run against
/// an already-parsed [`super::Robot`]; [`super::Robot::validate`] calls it
/// for every visual to confirm every bare reference actually resolves.
#[derive(Debug, Clone, PartialEq)]
pub struct Material {
    /// The material's name. Required by URDF on every `<material>`
    /// element, robot-level or inline.
    pub name: String,
    /// This material's color, if given (a `<texture>`-only material has
    /// none).
    pub color: Option<Color>,
    /// This material's texture image path, if given.
    pub texture_filename: Option<String>,
}

impl Material {
    /// Builds a material with no color and no texture — just a name (the
    /// shape a `<material name="foo"/>` reference resolves *away from*,
    /// never the shape a fully-resolved [`super::Visual::material`] is
    /// left in).
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            color: None,
            texture_filename: None,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn white_is_the_default_color() {
        assert_eq!(Color::default(), Color::WHITE);
        assert_eq!(Color::WHITE, Color::new(1.0, 1.0, 1.0, 1.0));
    }

    #[test]
    fn named_has_no_color_or_texture() {
        let m = Material::named("blue");
        assert_eq!(m.name, "blue");
        assert_eq!(m.color, None);
        assert_eq!(m.texture_filename, None);
    }
}
