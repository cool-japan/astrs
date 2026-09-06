//! [`ModuleLoader`]: how [`super::expand`] turns a resolved `module:` path
//! into file text, without ever touching the process's current working
//! directory (blueprint §8.5: "relative to the including manifest's
//! directory ... pass a base-dir parameter, never cwd").
//!
//! [`super::expand`] resolves and normalizes every `module:` path itself
//! (see [`super::rewrite::normalize_path`]) — a loader only ever sees the
//! fully resolved path and hands back text, keeping the recursion logic
//! independent of *where* module manifests actually live.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A source of module manifest file contents, keyed by resolved path.
///
/// [`super::expand`] never reads a file directly — every `module:`
/// reference is resolved to a path (relative to the *including* manifest's
/// own directory, never the process's current working directory) and
/// handed to this trait. [`FsModuleLoader`] is the production
/// implementation; [`MemoryModuleLoader`] is for tests and for embedding
/// AstRS manifests that were never written to disk at all (e.g. generated
/// or fetched over the network).
///
/// Takes `&self` rather than `&mut self`: reading a file is not
/// inherently a mutating operation, and a caching loader can still use
/// interior mutability (a `RefCell`-backed cache, for example) without
/// this trait forcing every call site to thread a mutable borrow through
/// deep recursion.
pub trait ModuleLoader {
    /// Read the file at `path` (already fully resolved by the caller) as
    /// UTF-8 text.
    ///
    /// # Errors
    ///
    /// Returns an [`std::io::Error`] if `path` does not exist, is not
    /// readable, or is not valid UTF-8 (implementations are free to map
    /// their own failure modes onto whatever [`std::io::ErrorKind`] fits
    /// best; [`super::ExpandError::Io`] carries this error through
    /// unchanged).
    fn read_to_string(&self, path: &Path) -> std::io::Result<String>;
}

/// Loads module manifests directly from the filesystem via
/// [`std::fs::read_to_string`] — the loader production code (the `astrs`
/// CLI's `expand`/`validate` verbs) uses.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FsModuleLoader;

impl ModuleLoader for FsModuleLoader {
    fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }
}

/// An in-memory [`ModuleLoader`] mapping paths to their YAML text.
///
/// Used by this crate's own tests (module composition, cycle detection,
/// and depth-limit fixtures do not need real files on disk to exercise
/// the expansion algorithm) and available to any caller that already has
/// module manifests as in-memory strings — a manifest fetched over the
/// network or generated programmatically, for example — with no temporary
/// file needed just to satisfy [`ModuleLoader`]'s path-based interface.
///
/// # Examples
///
/// ```
/// use astrs_manifest::expand::{MemoryModuleLoader, ModuleLoader};
/// use std::path::Path;
///
/// let loader = MemoryModuleLoader::new().with_file("/graphs/leaf.yaml", "module:\n  name: leaf\n");
/// assert!(loader.read_to_string(Path::new("/graphs/leaf.yaml")).is_ok());
/// assert!(loader.read_to_string(Path::new("/graphs/missing.yaml")).is_err());
/// ```
#[derive(Debug, Default, Clone)]
pub struct MemoryModuleLoader {
    files: BTreeMap<PathBuf, String>,
}

impl MemoryModuleLoader {
    /// An empty loader with no files registered.
    #[must_use]
    pub fn new() -> Self {
        Self {
            files: BTreeMap::new(),
        }
    }

    /// Register a file's contents, builder-style.
    #[must_use]
    pub fn with_file(mut self, path: impl Into<PathBuf>, contents: impl Into<String>) -> Self {
        self.insert(path, contents);
        self
    }

    /// Register a file's contents in place.
    pub fn insert(&mut self, path: impl Into<PathBuf>, contents: impl Into<String>) {
        self.files.insert(path.into(), contents.into());
    }
}

impl ModuleLoader for MemoryModuleLoader {
    fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        self.files.get(path).cloned().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no such module file registered: `{}`", path.display()),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn memory_loader_returns_registered_contents() {
        let loader = MemoryModuleLoader::new().with_file("/root/leaf.yaml", "module:\n  name: x\n");
        let text = loader.read_to_string(Path::new("/root/leaf.yaml")).unwrap();
        assert_eq!(text, "module:\n  name: x\n");
    }

    #[test]
    fn memory_loader_errors_on_unregistered_path() {
        let loader = MemoryModuleLoader::new();
        let err = loader.read_to_string(Path::new("/nope.yaml")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn memory_loader_insert_mutates_in_place() {
        let mut loader = MemoryModuleLoader::new();
        loader.insert("/a.yaml", "module:\n  name: a\n");
        assert!(loader.read_to_string(Path::new("/a.yaml")).is_ok());
    }

    #[test]
    fn fs_loader_reads_a_real_temp_file() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-manifest-loader-test-{}-{}",
            std::process::id(),
            "fs_loader_reads_a_real_temp_file"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("module.yaml");
        std::fs::write(&path, "module:\n  name: leaf\n").unwrap();

        let loader = FsModuleLoader;
        let text = loader.read_to_string(&path).unwrap();
        assert_eq!(text, "module:\n  name: leaf\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_loader_errors_on_missing_file() {
        let loader = FsModuleLoader;
        let path = std::env::temp_dir().join("astrs-manifest-definitely-missing.yaml");
        assert!(loader.read_to_string(&path).is_err());
    }
}
