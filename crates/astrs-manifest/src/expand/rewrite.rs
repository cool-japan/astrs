//! Pure, allocation-light helpers [`super::expand`]'s recursion uses to
//! resolve `module:` paths and rewrite `source:` strings. Kept free of any
//! recursion/loading concerns of its own so each piece is independently
//! unit-testable.

use std::path::{Component, Path, PathBuf};

/// Lexically normalize `path`: drop `.` components and collapse `a/b/../c`
/// to `a/c`, **without** touching the filesystem (no symlink resolution, no
/// existence check — this is not [`std::fs::canonicalize`]).
///
/// A leading `..` that cannot be collapsed (nothing precedes it, or the
/// preceding component is itself unresolved `..`) is preserved as-is,
/// matching how a shell would leave `../../x` alone with nothing to pop
/// against. A root or drive-prefix component always stops upward
/// collapsing at that point, the same way `/../x` degrades to `/x` rather
/// than escaping the root.
///
/// This is [`super::expand`]'s only path-comparison primitive — module
/// paths are compared for cycle detection and hashed as `local_ids`-scoped
/// keys purely as normalized strings, never via `canonicalize` (which
/// would require the file to exist and would defeat
/// [`super::MemoryModuleLoader`]'s synthetic, filesystem-free paths).
#[must_use]
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut out: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // A `..` right after a root/prefix component has nowhere
                // to go above the root — drop it, the same way `/../x`
                // degrades to `/x` rather than escaping the root.
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(component),
            },
            other => out.push(other),
        }
    }
    out.into_iter().collect()
}

/// Resolve a `module:` field's raw string against `base_dir` and
/// normalize the result — the one path computation [`super::expand`]'s
/// recursion performs at every level, so cycle detection and the loader
/// both see the exact same key for "the same file".
#[must_use]
pub(crate) fn resolve_module_path(base_dir: &Path, module_field: &str) -> PathBuf {
    normalize_path(&base_dir.join(module_field))
}

/// Split `source` into its `node_id` and `rest` parts on the first `/`, the
/// same grammar [`crate::Manifest::validate`]'s reference resolver uses.
/// Returns `None` for a malformed reference (no `/`, or an empty side) —
/// [`super::expand`] leaves those untouched and lets
/// [`crate::Manifest::validate`] report them.
#[must_use]
pub(crate) fn split_reference(source: &str) -> Option<(&str, &str)> {
    let (node_id, rest) = source.split_once('/')?;
    if node_id.is_empty() || rest.is_empty() {
        return None;
    }
    Some((node_id, rest))
}

/// Rewrite `source`'s `node_id` part through `rename`, leaving everything
/// after the first `/` untouched, and leaving `source` completely
/// unchanged when it is a recognized `astrs/...` virtual source, is
/// malformed, or `rename` declines (returns `None`).
///
/// This is the workhorse for "prefix this reference if it points at a node
/// local to the scope being hoisted" — `rename` is given the *un-prefixed*
/// `node_id` and decides whether (and how) to rewrite it, so the same
/// helper serves both the plain per-node-id prefix (`|id| local_ids
/// .contains(id).then(|| format!("{prefix}.{id}"))`) and any other
/// substitution keyed purely on the node-id part.
#[must_use]
pub(crate) fn rewrite_reference_node_id(
    source: &str,
    rename: impl FnOnce(&str) -> Option<String>,
) -> String {
    if crate::virtual_source::recognize(source).is_some() {
        return source.to_string();
    }
    let Some((node_id, rest)) = split_reference(source) else {
        return source.to_string();
    };
    match rename(node_id) {
        Some(new_id) => format!("{new_id}/{rest}"),
        None => source.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn normalize_drops_current_dir_components() {
        assert_eq!(
            normalize_path(Path::new("/a/./b/./c")),
            PathBuf::from("/a/b/c")
        );
    }

    #[test]
    fn normalize_collapses_parent_dir_against_a_normal_component() {
        assert_eq!(
            normalize_path(Path::new("/a/b/../c")),
            PathBuf::from("/a/c")
        );
        assert_eq!(
            normalize_path(Path::new("/a/b/c/../../d")),
            PathBuf::from("/a/d")
        );
    }

    #[test]
    fn normalize_keeps_unresolvable_leading_parent_dirs() {
        assert_eq!(
            normalize_path(Path::new("../../x")),
            PathBuf::from("../../x")
        );
    }

    #[test]
    fn normalize_does_not_escape_root() {
        assert_eq!(normalize_path(Path::new("/../x")), PathBuf::from("/x"));
    }

    #[test]
    fn normalize_is_idempotent() {
        let once = normalize_path(Path::new("/a/b/../../c/./d"));
        let twice = normalize_path(&once);
        assert_eq!(once, twice);
        assert_eq!(once, PathBuf::from("/c/d"));
    }

    #[test]
    fn resolve_module_path_joins_and_normalizes() {
        let resolved = resolve_module_path(Path::new("/graphs/perception"), "../modules/leaf.yaml");
        assert_eq!(resolved, PathBuf::from("/graphs/modules/leaf.yaml"));
    }

    #[test]
    fn resolve_module_path_handles_plain_relative_reference() {
        let resolved = resolve_module_path(Path::new("/graphs"), "./sub-graph.yaml");
        assert_eq!(resolved, PathBuf::from("/graphs/sub-graph.yaml"));
    }

    #[test]
    fn two_different_relative_spellings_of_the_same_file_normalize_equal() {
        let a = resolve_module_path(Path::new("/graphs"), "./sub/../leaf.yaml");
        let b = resolve_module_path(Path::new("/graphs"), "leaf.yaml");
        assert_eq!(a, b);
    }

    #[test]
    fn split_reference_splits_on_first_slash() {
        assert_eq!(split_reference("camera/frames"), Some(("camera", "frames")));
        assert_eq!(
            split_reference("host.child/_mod/detections"),
            Some(("host.child", "_mod/detections"))
        );
    }

    #[test]
    fn split_reference_rejects_malformed_forms() {
        assert_eq!(split_reference("malformed"), None);
        assert_eq!(split_reference("/frames"), None);
        assert_eq!(split_reference("camera/"), None);
    }

    #[test]
    fn rewrite_reference_node_id_applies_rename_when_it_matches() {
        let out = rewrite_reference_node_id("detector/detections", |id| {
            (id == "detector").then(|| format!("perception.{id}"))
        });
        assert_eq!(out, "perception.detector/detections");
    }

    #[test]
    fn rewrite_reference_node_id_leaves_non_matching_references_untouched() {
        let out = rewrite_reference_node_id("camera/frames", |id| {
            (id == "detector").then(|| format!("perception.{id}"))
        });
        assert_eq!(out, "camera/frames");
    }

    #[test]
    fn rewrite_reference_node_id_never_touches_virtual_sources() {
        let out = rewrite_reference_node_id("astrs/timer/hz/50", |_| {
            Some("should-not-be-used".to_string())
        });
        assert_eq!(out, "astrs/timer/hz/50");
    }

    #[test]
    fn rewrite_reference_node_id_leaves_malformed_references_untouched() {
        let out = rewrite_reference_node_id("malformed", |_| Some("x".to_string()));
        assert_eq!(out, "malformed");
    }

    #[test]
    fn rewrite_reference_node_id_preserves_a_multi_segment_rest() {
        let out = rewrite_reference_node_id("host/_mod/detections", |id| {
            (id == "host").then(|| "outer.host".to_string())
        });
        assert_eq!(out, "outer.host/_mod/detections");
    }
}
