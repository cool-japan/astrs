//! Resource ceilings the parser refuses to cross.
//!
//! A YAML parser is an attack surface: manifests arrive from a Git checkout,
//! from `astrs migrate`, from a hub package someone else published. Three
//! shapes of hostile (or merely accidental) input can turn a small file into
//! an unbounded amount of work, and [`Limits`] caps all three:
//!
//! | Input shape | Cap |
//! |---|---|
//! | A huge file | [`Limits::max_input_bytes`] |
//! | `[[[[[[…` — nesting that recurses the parser | [`Limits::max_depth`] |
//! | The "billion laughs" anchor bomb | [`Limits::max_alias_nodes`] |
//!
//! The defaults are chosen so that no legitimate dataflow manifest can hit
//! them: the largest manifest in this workspace is a few kilobytes and
//! nests four collections deep. They are *not* chosen to match any other
//! parser — a document rejected on a limit is refused deliberately, and
//! [`crate::Error::is_limit`] lets a caller tell that apart from "this file
//! is malformed".

/// Ceilings applied to one parse.
///
/// # Examples
///
/// ```
/// use astrs_yaml::{Limits, Value};
///
/// // A deliberately shallow parser: two levels of nesting, no more.
/// let limits = Limits::default().with_max_depth(2);
/// assert!(astrs_yaml::from_str_with::<Value>("a: [1, 2]", limits).is_ok());
/// let too_deep = astrs_yaml::from_str_with::<Value>("a: [[1]]", limits)
///     .unwrap_err();
/// assert!(too_deep.is_limit());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Limits {
    /// Largest accepted input, in bytes. Defaults to
    /// [`Limits::DEFAULT_MAX_INPUT_BYTES`] (64 MiB).
    ///
    /// Checked before a single character is scanned, so an oversized input
    /// costs one comparison rather than a parse.
    pub max_input_bytes: usize,

    /// Deepest accepted collection nesting. Defaults to
    /// [`Limits::DEFAULT_MAX_DEPTH`] (128).
    ///
    /// Counted on entry to *every* sequence or mapping, block or flow,
    /// before the recursive call — so `[[[[…` is refused at character 129,
    /// not after the stack is gone. 128 is also the depth at which
    /// `serde_yaml` stops, which keeps the two parsers agreeing on which
    /// documents are acceptable.
    pub max_depth: usize,

    /// Total number of nodes alias expansion may materialize. Defaults to
    /// [`Limits::DEFAULT_MAX_ALIAS_NODES`] (1 000 000).
    ///
    /// Every `*alias` clones the anchored subtree; the size of that subtree
    /// is charged against this budget. A "billion laughs" bomb multiplies
    /// its anchors by roughly ten per level, so the budget is exhausted
    /// after nine levels regardless of how many the document declares.
    pub max_alias_nodes: usize,
}

impl Limits {
    /// Default value of [`Limits::max_input_bytes`]: 64 MiB.
    pub const DEFAULT_MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;

    /// Default value of [`Limits::max_depth`]: 128 nested collections.
    pub const DEFAULT_MAX_DEPTH: usize = 128;

    /// Default value of [`Limits::max_alias_nodes`]: one million nodes.
    pub const DEFAULT_MAX_ALIAS_NODES: usize = 1_000_000;

    /// The default ceilings, as a `const` so they can seed a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_input_bytes: Self::DEFAULT_MAX_INPUT_BYTES,
            max_depth: Self::DEFAULT_MAX_DEPTH,
            max_alias_nodes: Self::DEFAULT_MAX_ALIAS_NODES,
        }
    }

    /// Return these limits with [`Limits::max_input_bytes`] replaced.
    #[must_use]
    pub const fn with_max_input_bytes(mut self, bytes: usize) -> Self {
        self.max_input_bytes = bytes;
        self
    }

    /// Return these limits with [`Limits::max_depth`] replaced.
    #[must_use]
    pub const fn with_max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }

    /// Return these limits with [`Limits::max_alias_nodes`] replaced.
    #[must_use]
    pub const fn with_max_alias_nodes(mut self, nodes: usize) -> Self {
        self.max_alias_nodes = nodes;
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
        assert_eq!(limits.max_input_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.max_depth, 128);
        assert_eq!(limits.max_alias_nodes, 1_000_000);
        assert_eq!(limits, Limits::new());
    }

    #[test]
    fn builders_replace_exactly_one_field_each() {
        let base = Limits::new();
        assert_eq!(base.with_max_depth(3).max_depth, 3);
        assert_eq!(base.with_max_depth(3).max_input_bytes, base.max_input_bytes);
        assert_eq!(base.with_max_input_bytes(7).max_input_bytes, 7);
        assert_eq!(base.with_max_alias_nodes(9).max_alias_nodes, 9);
        assert_eq!(base.with_max_alias_nodes(9).max_depth, base.max_depth);
    }
}
