//! `validate`/`expand`/`graph` against the blueprint §8.1 canonical
//! example manifest — this crate's own committed copy (not a
//! cross-reference into `astrs-manifest`'s fixtures, which are that
//! crate's to own and may reasonably not exist at the exact same path
//! forever).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use astrs_cli::command::{expand, graph, validate};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/perception_demo.yaml")
}

#[test]
fn validate_reports_the_example_as_completely_clean() {
    let mut out = Vec::new();
    let report = validate::run(
        &mut out,
        &validate::ValidateArgs {
            manifest_path: fixture(),
            prove: false,
            profile: None,
            json: false,
            color: false,
        },
    )
    .unwrap();
    assert!(
        report.diagnostics.is_empty(),
        "blueprint §8.1's own example should validate clean; diagnostics: {:?}",
        report.diagnostics
    );
    assert_eq!(report.exit_code(), 0);
    let printed = String::from_utf8(out).unwrap();
    assert!(printed.contains("clean"));
}

#[test]
fn validate_prove_runs_the_graph_obligations_on_the_example() {
    // `--prove` never suppresses or alters the ordinary pipeline's own
    // findings -- the manifest is otherwise spotless, and that still shows
    // up -- it appends the proof report after them.
    let mut out = Vec::new();
    let outcome = validate::run(
        &mut out,
        &validate::ValidateArgs {
            manifest_path: fixture(),
            prove: true,
            profile: None,
            json: false,
            color: false,
        },
    );

    let printed = String::from_utf8(out).unwrap();
    assert!(printed.contains("graph proofs"), "{printed}");

    match outcome {
        Ok(report) => {
            // A build with the `verify` feature: the obligations were
            // actually discharged.
            let proof = report.proof.as_ref().expect("a proof was run");
            assert!(
                !proof.has_violations(),
                "blueprint §8.1's own example must not be provably broken:\n{printed}"
            );
            assert!(proof.node_count > 0);
        }
        Err(err) => {
            // A build without it: every obligation was still encoded, and
            // the missing solver is reported by exit code alone.
            assert!(
                matches!(err, astrs_cli::CliError::ProofUnavailable),
                "expected ProofUnavailable, got {err:?}"
            );
            assert_eq!(err.exit_code(), astrs_cli::error::EXIT_UNAVAILABLE);
            assert!(printed.contains("--features verify"), "{printed}");
        }
    }

    // No *other* diagnostic category fired -- the manifest itself is
    // still otherwise clean.
    for other_source in ["(io)", "(parse)", "(structural)", "(expand)"] {
        assert!(
            !printed.contains(other_source),
            "unexpected {other_source} in:\n{printed}"
        );
    }
}

#[test]
fn expand_is_a_no_op_since_the_example_has_no_modules() {
    let mut out = Vec::new();
    let report = expand::run(
        &mut out,
        &expand::ExpandArgs {
            manifest_path: fixture(),
        },
    )
    .unwrap();
    assert_eq!(report.manifest.nodes.len(), 4);
    let ids: Vec<_> = report
        .manifest
        .nodes
        .iter()
        .map(|n| n.id.as_str())
        .collect();
    assert_eq!(ids, vec!["camera", "detector", "recorder", "planner"]);
}

#[test]
fn graph_reports_the_exact_node_and_edge_set() {
    // `graph::run`'s own `GraphReport` exposes only rendered text plus
    // counts (see its own doc comment) -- reconstruct the same
    // `DataflowGraph` directly here too, so this test pins down not just
    // *how many* edges exist but *which* ones: detector<-camera,
    // recorder<-camera / recorder<-detector (the `record:` sugar,
    // synthesized as `_record/0`/`_record/1` -- see
    // `astrs_graph::DataflowGraph::from_manifest`'s own handling of it),
    // planner<-detector, and planner's virtual timer input.
    let mut out = Vec::new();
    let report = graph::run(
        &mut out,
        &graph::GraphArgs {
            manifest_path: fixture(),
            format: graph::GraphFormat::Mermaid,
        },
    )
    .unwrap();
    assert_eq!(report.node_count, 4);
    assert_eq!(report.edge_count, 5);

    let content = std::fs::read_to_string(fixture()).unwrap();
    let manifest = astrs_manifest::Manifest::from_yaml_str(&content).unwrap();
    manifest.validate().unwrap();
    let expanded = manifest
        .expand(Path::new("."), &astrs_manifest::expand::FsModuleLoader)
        .unwrap();
    let (graph, construction_diagnostics) =
        astrs_graph::DataflowGraph::from_manifest(&expanded).unwrap();
    assert!(construction_diagnostics.is_empty());

    // The exact node set: `node_count() == 4` plus every one of these
    // four ids resolving is already exhaustive -- a fifth, unaccounted
    // node cannot exist without the count disagreeing.
    for id in ["camera", "detector", "recorder", "planner"] {
        assert!(
            graph.node(&astrs_graph::NodeId::new(id)).is_some(),
            "missing node `{id}`"
        );
        assert!(
            report.rendered.contains(id),
            "missing {id} in:\n{}",
            report.rendered
        );
    }

    let edge_from = |consumer: &str, input: &str| -> String {
        graph
            .edge(&astrs_graph::EdgeKey::new(
                astrs_graph::NodeId::new(consumer),
                astrs_graph::PortName::new(input),
            ))
            .map(|e| e.from.to_string())
            .unwrap_or_else(|| panic!("no edge `{consumer}.{input}`"))
    };
    assert_eq!(edge_from("detector", "frames"), "camera.frames");
    assert_eq!(edge_from("recorder", "_record/0"), "camera.frames");
    assert_eq!(edge_from("recorder", "_record/1"), "detector.detections");
    assert_eq!(edge_from("planner", "detections"), "detector.detections");
    assert_eq!(edge_from("planner", "tick"), "astrs/timer/hz/50");

    // Every one of the 5 edges `edge_count()` reports is accounted for by
    // the checks above: 0 (camera) + 1 (detector) + 2 (recorder) + 2
    // (planner) == 5, so none of these four nodes has an extra,
    // unchecked incoming edge either.
    let incoming = |id: &str| graph.edges_into(&astrs_graph::NodeId::new(id)).count();
    assert_eq!(incoming("camera"), 0);
    assert_eq!(incoming("detector"), 1);
    assert_eq!(incoming("recorder"), 2);
    assert_eq!(incoming("planner"), 2);
}

#[test]
fn graph_html_format_embeds_every_node_id() {
    let mut out = Vec::new();
    let report = graph::run(
        &mut out,
        &graph::GraphArgs {
            manifest_path: fixture(),
            format: graph::GraphFormat::Html,
        },
    )
    .unwrap();
    assert!(report.rendered.starts_with("<!doctype html>"));
    for id in ["camera", "detector", "recorder", "planner"] {
        assert!(report.rendered.contains(id));
    }
}
