//! `package.xml` parsing and two ways of finding ROS 2 packages on disk
//! (blueprint §10.3): an ament-index install tree (`AMENT_PREFIX_PATH`
//! entries — what a sourced ROS 2 workspace sets) and a colcon-style source
//! tree (a directory containing packages nested arbitrarily deep, the
//! layout `astrs`'s own `msg-src/` regeneration fixtures use).
//!
//! Neither path is required for the common case this crate exists to make
//! unnecessary: [`crate::generated`]'s pre-generated `common_interfaces` set
//! needs no ROS files on disk at all. This module is for the *next* case —
//! a package outside that set — and for the regeneration pipeline that
//! keeps `common_interfaces` itself honest against real `.msg`/`.srv`/
//! `.action` text.

use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::reader::Reader;

use crate::error::IdlError;
use crate::naming::{InterfaceKind, PackageName};
use crate::span::{Position, Span};

/// One `.msg`/`.srv`/`.action` file found under a package directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceFile {
    /// `msg`, `srv` or `action` — which subdirectory it was found under.
    pub kind: InterfaceKind,
    /// The file name without its extension (`"Point"` for `msg/Point.msg`) —
    /// not yet validated as a legal ROS 2 type name; [`crate::naming::TypeName::new`]
    /// does that when this file is actually parsed.
    pub type_name: String,
    /// The file's full path.
    pub path: PathBuf,
}

/// A package located on disk: its validated name, directory, and every
/// interface file found under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPackage {
    /// The name `package.xml` declares.
    pub name: PackageName,
    /// The package's root directory (containing `package.xml`).
    pub dir: PathBuf,
    /// Every `.msg`/`.srv`/`.action` file found under `dir`.
    pub interfaces: Vec<InterfaceFile>,
}

/// Parses `package.xml` at `path` and returns its `<name>` element,
/// validated as a [`PackageName`].
///
/// Only a direct child of the root `<package>` element is accepted — a
/// `<name>` nested inside `<export>` or similar (package.xml has none in
/// practice, but nothing rules it out syntactically) is ignored, so the
/// first `<name>` encountered at any depth cannot be the wrong one.
///
/// # Errors
///
/// [`IdlError::Io`] if `path` cannot be read, [`IdlError::PackageXmlParse`]
/// if it is not well-formed XML, [`IdlError::PackageXmlMissingName`] if no
/// direct-child `<name>` element is found, [`IdlError::PackageXmlInvalidName`]
/// if its text is not a valid ROS 2 package name.
pub fn parse_package_xml(path: &Path) -> Result<PackageName, IdlError> {
    let content = std::fs::read_to_string(path).map_err(|source| IdlError::Io {
        path: path.to_path_buf(),
        message: source.to_string(),
    })?;

    let mut reader = Reader::from_str(&content);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut depth: u32 = 0;
    let mut in_top_level_name = false;
    let mut name_text: Option<String> = None;

    loop {
        let event =
            reader
                .read_event_into(&mut buf)
                .map_err(|source| IdlError::PackageXmlParse {
                    path: path.to_path_buf(),
                    message: source.to_string(),
                })?;
        match event {
            Event::Eof => break,
            Event::Start(start) => {
                depth += 1;
                if depth == 2 && start.name().as_ref() == b"name" && name_text.is_none() {
                    in_top_level_name = true;
                }
            }
            // A self-closing `<name/>` (empty text) — still counts as
            // "found", just with empty text, which `PackageName::new` will
            // reject with a clear error rather than this function silently
            // falling through to `PackageXmlMissingName`.
            Event::Empty(empty)
                if depth + 1 == 2 && empty.name().as_ref() == b"name" && name_text.is_none() =>
            {
                name_text = Some(String::new());
            }
            Event::Text(text) if in_top_level_name => {
                let decoded = text.decode().map_err(|source| IdlError::PackageXmlParse {
                    path: path.to_path_buf(),
                    message: source.to_string(),
                })?;
                name_text = Some(decoded.into_owned());
            }
            Event::End(end) => {
                if depth == 2 && end.name().as_ref() == b"name" {
                    in_top_level_name = false;
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        buf.clear();
    }

    let name_text = name_text.ok_or_else(|| IdlError::PackageXmlMissingName {
        path: path.to_path_buf(),
    })?;
    PackageName::new(name_text.clone(), Span::empty(Position::START)).map_err(|_source| {
        IdlError::PackageXmlInvalidName {
            path: path.to_path_buf(),
            name: name_text,
        }
    })
}

/// Lists every `.msg`/`.srv`/`.action` file directly under
/// `package_dir/{msg,srv,action}/`.
///
/// # Errors
///
/// [`IdlError::Io`] if a `msg`/`srv`/`action` subdirectory exists but cannot
/// be read (a missing subdirectory — the common case, most packages have no
/// services or no actions — is not an error).
pub fn discover_interface_files(package_dir: &Path) -> Result<Vec<InterfaceFile>, IdlError> {
    let mut files = Vec::new();
    for kind in [
        InterfaceKind::Msg,
        InterfaceKind::Srv,
        InterfaceKind::Action,
    ] {
        let subdir = package_dir.join(kind.as_str());
        if !subdir.is_dir() {
            continue;
        }
        let entries = std::fs::read_dir(&subdir).map_err(|source| IdlError::Io {
            path: subdir.clone(),
            message: source.to_string(),
        })?;
        let mut found: Vec<InterfaceFile> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| IdlError::Io {
                path: subdir.clone(),
                message: source.to_string(),
            })?;
            let path = entry.path();
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some(kind.extension()) {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(std::ffi::OsStr::to_str) else {
                continue;
            };
            found.push(InterfaceFile {
                kind,
                type_name: stem.to_owned(),
                path: path.clone(),
            });
        }
        found.sort_by(|a, b| a.type_name.cmp(&b.type_name));
        files.extend(found);
    }
    Ok(files)
}

/// Discovers one package: its `package.xml` and every interface file under
/// it.
///
/// # Errors
///
/// [`IdlError::PackageXmlNotFound`] if `package_dir/package.xml` does not
/// exist, plus anything [`parse_package_xml`]/[`discover_interface_files`]
/// raise.
pub fn discover_package(package_dir: &Path) -> Result<DiscoveredPackage, IdlError> {
    let manifest = package_dir.join("package.xml");
    if !manifest.is_file() {
        return Err(IdlError::PackageXmlNotFound {
            path: package_dir.to_path_buf(),
        });
    }
    let name = parse_package_xml(&manifest)?;
    let interfaces = discover_interface_files(package_dir)?;
    Ok(DiscoveredPackage {
        name,
        dir: package_dir.to_path_buf(),
        interfaces,
    })
}

/// Walks a colcon-style source tree (packages nested arbitrarily deep under
/// `root`, the layout a checked-out ROS workspace `src/` directory has) and
/// discovers every package found.
///
/// # Errors
///
/// Whatever [`discover_package`] raises for any `package.xml` found; a
/// malformed pattern from the (fixed, `**/package.xml`) glob itself is
/// unreachable and would indicate a bug in this function.
pub fn discover_source_tree(root: &Path) -> Result<Vec<DiscoveredPackage>, IdlError> {
    let pattern = format!("{}/**/package.xml", root.display());
    let mut packages = Vec::new();
    let paths = glob::glob(&pattern).map_err(|source| IdlError::Io {
        path: root.to_path_buf(),
        message: source.to_string(),
    })?;
    for entry in paths {
        let manifest = entry.map_err(|source| IdlError::Io {
            path: root.to_path_buf(),
            message: source.to_string(),
        })?;
        let Some(package_dir) = manifest.parent() else {
            continue;
        };
        packages.push(discover_package(package_dir)?);
    }
    packages.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    Ok(packages)
}

/// Discovers every package installed under one ament index prefix (one
/// entry of `AMENT_PREFIX_PATH`): every name marked in
/// `<prefix>/share/ament_index/resource_index/packages/`, each resolved to
/// `<prefix>/share/<name>/`.
///
/// # Errors
///
/// [`IdlError::Io`] if the resource index directory cannot be read, plus
/// whatever [`discover_package`] raises for a marked package whose
/// `<prefix>/share/<name>/` turns out not to exist or not to carry a valid
/// `package.xml`.
pub fn discover_ament_prefix(prefix: &Path) -> Result<Vec<DiscoveredPackage>, IdlError> {
    let index_dir = prefix.join("share/ament_index/resource_index/packages");
    if !index_dir.is_dir() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(&index_dir).map_err(|source| IdlError::Io {
        path: index_dir.clone(),
        message: source.to_string(),
    })?;
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| IdlError::Io {
            path: index_dir.clone(),
            message: source.to_string(),
        })?;
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_owned());
        }
    }
    names.sort();
    names
        .into_iter()
        .map(|name| discover_package(&prefix.join("share").join(name)))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "astrs-idl-discovery-test-{label}-{}-{}",
            std::process::id(),
            uuid_ish()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A cheap, dependency-free unique suffix — this crate has no `uuid`
    /// dependency, and a monotonically increasing counter is all a
    /// same-process temp-directory name needs to avoid colliding with a
    /// previous test run.
    fn uuid_ish() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn parse_package_xml_reads_the_top_level_name() {
        let dir = temp_dir("name");
        let manifest = dir.join("package.xml");
        write(
            &manifest,
            r#"<?xml version="1.0"?>
<package format="3">
  <name>geometry_msgs</name>
  <version>4.2.3</version>
</package>"#,
        );
        let name = parse_package_xml(&manifest).unwrap();
        assert_eq!(name.as_str(), "geometry_msgs");
    }

    #[test]
    fn parse_package_xml_ignores_a_nested_name_and_uses_the_top_level_one() {
        let dir = temp_dir("nested-name");
        let manifest = dir.join("package.xml");
        // `<export>` blocks can carry arbitrary child elements; a
        // pathological one named `<name>` must not be mistaken for the
        // package's own name.
        write(
            &manifest,
            r#"<package format="3">
  <export><name>not_the_package_name</name></export>
  <name>real_name</name>
</package>"#,
        );
        let name = parse_package_xml(&manifest).unwrap();
        assert_eq!(name.as_str(), "real_name");
    }

    #[test]
    fn parse_package_xml_rejects_missing_file() {
        let dir = temp_dir("missing");
        let err = parse_package_xml(&dir.join("package.xml")).unwrap_err();
        assert!(matches!(err, IdlError::Io { .. }));
    }

    #[test]
    fn parse_package_xml_rejects_a_missing_name_element() {
        let dir = temp_dir("no-name");
        let manifest = dir.join("package.xml");
        write(
            &manifest,
            r#"<package format="3"><version>1.0.0</version></package>"#,
        );
        let err = parse_package_xml(&manifest).unwrap_err();
        assert!(matches!(err, IdlError::PackageXmlMissingName { .. }));
    }

    #[test]
    fn parse_package_xml_rejects_an_invalid_name() {
        let dir = temp_dir("invalid-name");
        let manifest = dir.join("package.xml");
        write(
            &manifest,
            r#"<package format="3"><name>Not-Valid!</name></package>"#,
        );
        let err = parse_package_xml(&manifest).unwrap_err();
        assert!(matches!(err, IdlError::PackageXmlInvalidName { .. }));
    }

    #[test]
    fn parse_package_xml_rejects_malformed_xml() {
        let dir = temp_dir("malformed");
        let manifest = dir.join("package.xml");
        write(&manifest, "<package><name>oops</name");
        let err = parse_package_xml(&manifest).unwrap_err();
        assert!(matches!(err, IdlError::PackageXmlParse { .. }));
    }

    #[test]
    fn discover_interface_files_finds_msg_srv_and_action_and_skips_other_extensions() {
        let dir = temp_dir("interfaces");
        write(&dir.join("msg/Point.msg"), "float64 x\n");
        write(&dir.join("msg/README.md"), "not an interface\n");
        write(&dir.join("srv/Trigger.srv"), "---\nbool success\n");
        write(
            &dir.join("action/Fibonacci.action"),
            "int32 order\n---\n---\n",
        );
        let files = discover_interface_files(&dir).unwrap();
        let names: Vec<(InterfaceKind, &str)> = files
            .iter()
            .map(|f| (f.kind, f.type_name.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                (InterfaceKind::Msg, "Point"),
                (InterfaceKind::Srv, "Trigger"),
                (InterfaceKind::Action, "Fibonacci"),
            ]
        );
    }

    #[test]
    fn discover_interface_files_on_a_package_with_no_interfaces_is_empty_not_an_error() {
        let dir = temp_dir("no-interfaces");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(discover_interface_files(&dir).unwrap(), Vec::new());
    }

    #[test]
    fn discover_package_combines_the_manifest_and_the_interface_files() {
        let dir = temp_dir("full-package");
        write(
            &dir.join("package.xml"),
            "<package><name>demo_msgs</name></package>",
        );
        write(&dir.join("msg/Thing.msg"), "int32 value\n");
        let package = discover_package(&dir).unwrap();
        assert_eq!(package.name.as_str(), "demo_msgs");
        assert_eq!(package.interfaces.len(), 1);
    }

    #[test]
    fn discover_package_without_a_manifest_is_reported() {
        let dir = temp_dir("no-manifest");
        std::fs::create_dir_all(&dir).unwrap();
        let err = discover_package(&dir).unwrap_err();
        assert!(matches!(err, IdlError::PackageXmlNotFound { .. }));
    }

    #[test]
    fn discover_source_tree_finds_nested_packages() {
        let root = temp_dir("source-tree");
        write(
            &root.join("a_pkg/package.xml"),
            "<package><name>a_pkg</name></package>",
        );
        write(
            &root.join("nested/deep/b_pkg/package.xml"),
            "<package><name>b_pkg</name></package>",
        );
        let packages = discover_source_tree(&root).unwrap();
        let names: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["a_pkg", "b_pkg"]);
    }

    #[test]
    fn discover_ament_prefix_resolves_marked_packages() {
        let prefix = temp_dir("ament-prefix");
        write(
            &prefix.join("share/ament_index/resource_index/packages/geometry_msgs"),
            "",
        );
        write(
            &prefix.join("share/geometry_msgs/package.xml"),
            "<package><name>geometry_msgs</name></package>",
        );
        write(
            &prefix.join("share/geometry_msgs/msg/Point.msg"),
            "float64 x\n",
        );
        let packages = discover_ament_prefix(&prefix).unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name.as_str(), "geometry_msgs");
        assert_eq!(packages[0].interfaces.len(), 1);
    }

    #[test]
    fn discover_ament_prefix_with_no_resource_index_is_empty_not_an_error() {
        let prefix = temp_dir("ament-empty");
        std::fs::create_dir_all(&prefix).unwrap();
        assert_eq!(discover_ament_prefix(&prefix).unwrap(), Vec::new());
    }
}
