//! `astrs expand` (blueprint §8.5, §17): print a manifest with every
//! `module:`-sourced node flattened, in stable YAML — the shape
//! `astrs-graph` and the runtime actually consume.

use std::io::Write;
use std::path::{Path, PathBuf};

use astrs_manifest::Manifest;
use astrs_manifest::expand::FsModuleLoader;

use crate::error::CliError;

/// Arguments for `astrs expand`.
#[derive(Debug, Clone)]
pub struct ExpandArgs {
    /// The manifest file to expand.
    pub manifest_path: PathBuf,
}

/// The result of expanding one manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpandReport {
    /// The flattened manifest.
    pub manifest: Manifest,
    /// [`Self::manifest`] rendered as YAML — exactly what was written to
    /// the output sink.
    pub yaml: String,
}

/// Run `astrs expand`: parse, validate, expand, and print the flattened
/// manifest as YAML.
///
/// Unlike `validate`, a manifest that fails any of these steps aborts the
/// command outright — there is no meaningful "flattened YAML" to print
/// for a manifest that never became valid, so this is one of the few
/// commands where an upstream failure becomes this function's own `Err`
/// rather than report content (see `crate::error` module docs).
///
/// # Errors
///
/// Returns [`CliError::Io`] if the file cannot be read (or the output
/// sink cannot be written to), [`CliError::Manifest`] if it does not
/// parse, [`CliError::Validation`] if it fails structural validation
/// (checked both before and after expansion — see `command::validate`'s
/// own docs for why), or [`CliError::Expand`] if module expansion itself
/// fails.
pub fn run(out: &mut dyn Write, args: &ExpandArgs) -> Result<ExpandReport, CliError> {
    let content = std::fs::read_to_string(&args.manifest_path)
        .map_err(|err| CliError::io(&args.manifest_path, err))?;
    let manifest = Manifest::from_yaml_str(&content)?;
    manifest.validate()?;

    let base_dir = base_dir_of(&args.manifest_path);
    let loader = FsModuleLoader;
    let expanded = manifest.expand(&base_dir, &loader)?;
    expanded.validate()?;

    let yaml = expanded.to_yaml().map_err(CliError::Manifest)?;
    writeln!(out, "{yaml}").map_err(|e| CliError::io("<output>", e))?;

    Ok(ExpandReport {
        manifest: expanded,
        yaml,
    })
}

/// See `command::validate::base_dir_of` — duplicated rather than shared
/// because it is a two-line, crate-visibility-free helper and both call
/// sites want it `private` to their own module for independent testing.
fn base_dir_of(manifest_path: &Path) -> PathBuf {
    match manifest_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn write_temp(name: &str, content: &str) -> PathBuf {
        // A monotonic counter, not just `(pid, name)`, disambiguates the
        // directory -- see `command::graph`'s own test helper for the
        // observed race this guards against if a future test ever reuses
        // an existing `name` under cargo's multi-threaded test runner.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "astrs-cli-expand-test-{}-{name}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("root.yaml");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn a_manifest_with_no_modules_is_a_no_op() {
        let path = write_temp("no-modules", "nodes:\n  - id: solo\n    path: ./solo\n");
        let mut out = Vec::new();
        let report = run(
            &mut out,
            &ExpandArgs {
                manifest_path: path,
            },
        )
        .unwrap();
        assert_eq!(report.manifest.nodes.len(), 1);
        let printed = String::from_utf8(out).unwrap();
        // Round-trips through the manifest parser to the identical
        // structure -- proving `expand`'s printed text really is what
        // `ExpandReport::yaml` says it is, without pinning the exact
        // bytes `Manifest::to_yaml` happens to emit today (that crate's
        // own tests already freeze its formatting).
        let reparsed = Manifest::from_yaml_str(printed.trim_end()).unwrap();
        assert_eq!(reparsed, report.manifest);
    }

    #[test]
    fn a_module_reference_is_flattened_and_disappears() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-cli-expand-test-{}-module",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("root.yaml"),
            "nodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames]\n  - id: m\n    module: ./perception.yaml\n    inputs:\n      frames: camera/frames\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("perception.yaml"),
            "module:\n  name: perception\n  inputs: [frames]\nnodes:\n  - id: detector\n    path: ./detector\n    inputs:\n      frames: _mod/frames\n",
        )
        .unwrap();

        let mut out = Vec::new();
        let report = run(
            &mut out,
            &ExpandArgs {
                manifest_path: dir.join("root.yaml"),
            },
        )
        .unwrap();
        let ids: Vec<_> = report
            .manifest
            .nodes
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(ids, vec!["camera", "m.detector"]);
        assert!(report.manifest.module.is_none());
        assert!(!report.yaml.contains("module:"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_yaml_is_a_manifest_error() {
        let path = write_temp("bad-yaml", "nodes: [");
        let mut out = Vec::new();
        let err = run(
            &mut out,
            &ExpandArgs {
                manifest_path: path,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CliError::Manifest(_)));
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let path = std::env::temp_dir().join("astrs-cli-expand-does-not-exist.yaml");
        let mut out = Vec::new();
        let err = run(
            &mut out,
            &ExpandArgs {
                manifest_path: path,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CliError::Io { .. }));
    }

    #[test]
    fn structural_violation_is_a_validation_error() {
        let path = write_temp(
            "dup",
            "nodes:\n  - id: x\n    path: ./x\n  - id: x\n    path: ./y\n",
        );
        let mut out = Vec::new();
        let err = run(
            &mut out,
            &ExpandArgs {
                manifest_path: path,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CliError::Validation(_)));
    }

    #[test]
    fn missing_module_file_is_an_expand_error() {
        let path = write_temp(
            "missing-module",
            "nodes:\n  - id: m\n    module: ./missing.yaml\n",
        );
        let mut out = Vec::new();
        let err = run(
            &mut out,
            &ExpandArgs {
                manifest_path: path,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CliError::Expand(_)));
    }

    #[test]
    fn expand_output_is_stable_across_repeated_runs() {
        let path = write_temp("stable", "nodes:\n  - id: solo\n    path: ./solo\n");
        let mut first = Vec::new();
        let mut second = Vec::new();
        run(
            &mut first,
            &ExpandArgs {
                manifest_path: path.clone(),
            },
        )
        .unwrap();
        run(
            &mut second,
            &ExpandArgs {
                manifest_path: path,
            },
        )
        .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn base_dir_of_bare_filename_is_dot() {
        assert_eq!(base_dir_of(Path::new("root.yaml")), PathBuf::from("."));
    }
}
