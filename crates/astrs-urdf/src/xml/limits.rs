//! Resource ceilings [`super::Reader`] refuses to cross.
//!
//! A URDF file can arrive from anywhere a robot description can arrive from
//! — a Git checkout, a hub package, a ROS package downloaded at build time —
//! and two shapes of hostile (or merely accidental) input can turn a small
//! file into an unbounded amount of work: [`Limits`] caps both.
//!
//! | Input shape | Cap |
//! |---|---|
//! | A huge file | [`Limits::max_input_bytes`] |
//! | `<a><a><a><a>…` — nesting that recurses a naive parser | [`Limits::max_depth`] |
//!
//! This crate's own [`super::Reader`] is iterative (an explicit element
//! stack, no recursive descent over markup), so `max_depth` is not a stack
//! overflow guard here the way it would be for a recursive-descent parser —
//! it exists so a pathological document is refused with a precise
//! [`super::XmlErrorKind::DepthLimitExceeded`] rather than being accepted
//! and then handed to a *consumer* (this crate's own `parse` module
//! included) that walks the resulting tree recursively.

/// Ceilings applied to one parse.
///
/// # Examples
///
/// ```
/// use astrs_urdf::xml::{Limits, Reader};
///
/// let limits = Limits::default().with_max_depth(2);
/// let mut reader = Reader::with_limits("<a><b><c></c></b></a>", limits);
/// let error = loop {
///     match reader.next_event() {
///         Ok(astrs_urdf::xml::Event::Eof { .. }) => panic!("expected a depth-limit error"),
///         Ok(_) => continue,
///         Err(error) => break error,
///     }
/// };
/// assert!(matches!(
///     error.kind,
///     astrs_urdf::xml::XmlErrorKind::DepthLimitExceeded { limit: 2 }
/// ));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Limits {
    /// Largest accepted input, in bytes. Defaults to
    /// [`Limits::DEFAULT_MAX_INPUT_BYTES`] (32 MiB).
    ///
    /// Checked once, before a single character is scanned, so an oversized
    /// input costs one comparison rather than a parse.
    pub max_input_bytes: usize,

    /// Deepest accepted element nesting. Defaults to
    /// [`Limits::DEFAULT_MAX_DEPTH`] (256).
    ///
    /// Checked on every *non-self-closing* `StartElement`, before it is
    /// pushed onto the reader's own element stack — so `<a><a><a>…` is
    /// refused the moment depth 257 would be opened, not after the stack
    /// has grown unbounded. A self-closing `<a/>` is exempt: it has no
    /// children by construction (it is already closed), so — unlike a
    /// `<a>...</a>` pair — it can never itself deepen the nesting a
    /// downstream consumer would have to recurse through; only elements
    /// that can *contain something* count against this limit. A real URDF
    /// nests at most a handful of elements deep (`robot` > `link` >
    /// `visual`/`collision` > `geometry` > shape), so 256 leaves generous
    /// headroom without letting a hostile document recurse a downstream
    /// consumer arbitrarily deep.
    pub max_depth: usize,
}

impl Limits {
    /// Default value of [`Limits::max_input_bytes`]: 32 MiB.
    pub const DEFAULT_MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;

    /// Default value of [`Limits::max_depth`]: 256 nested elements.
    pub const DEFAULT_MAX_DEPTH: usize = 256;

    /// The default ceilings, as a `const` so they can seed a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_input_bytes: Self::DEFAULT_MAX_INPUT_BYTES,
            max_depth: Self::DEFAULT_MAX_DEPTH,
        }
    }

    /// Returns these limits with [`Limits::max_input_bytes`] replaced.
    #[must_use]
    pub const fn with_max_input_bytes(mut self, bytes: usize) -> Self {
        self.max_input_bytes = bytes;
        self
    }

    /// Returns these limits with [`Limits::max_depth`] replaced.
    #[must_use]
    pub const fn with_max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn the_defaults_are_the_documented_constants() {
        let limits = Limits::default();
        assert_eq!(limits.max_input_bytes, 32 * 1024 * 1024);
        assert_eq!(limits.max_depth, 256);
        assert_eq!(limits, Limits::new());
    }

    #[test]
    fn builders_replace_exactly_one_field_each() {
        let base = Limits::new();
        assert_eq!(base.with_max_depth(3).max_depth, 3);
        assert_eq!(base.with_max_depth(3).max_input_bytes, base.max_input_bytes);
        assert_eq!(base.with_max_input_bytes(7).max_input_bytes, 7);
        assert_eq!(base.with_max_input_bytes(7).max_depth, base.max_depth);
    }
}
