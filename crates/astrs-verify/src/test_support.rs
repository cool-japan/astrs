//! Shared fixtures for this crate's unit tests.
//!
//! Every obligation is tested against a *manifest*, not against a
//! hand-built model: an encoding that is right about a synthetic model and
//! wrong about the graph a real manifest produces has proved nothing. The
//! helpers here run the whole real pipeline — parse, validate, build the
//! graph, build the model — so a change in any of those crates shows up
//! here rather than in production.

#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_graph::DataflowGraph;
use astrs_manifest::Manifest;

use crate::model::Model;
use crate::profile::Profile;

/// Build a model from manifest YAML, with no verification profile.
///
/// # Panics
///
/// Panics if the YAML is not a valid, structurally sound manifest — a test
/// fixture that does not parse is a broken test, and failing loudly beats
/// silently exercising an empty graph.
#[must_use]
pub fn model_of(yaml: &str) -> Model {
    let manifest = Manifest::from_yaml_str(yaml).expect("fixture manifest must parse");
    manifest.validate().expect("fixture manifest must validate");
    let (graph, _) = DataflowGraph::from_manifest(&manifest).expect("fixture graph must build");
    Model::from_graph(&graph).expect("fixture model must build")
}

/// Build a model from manifest YAML plus profile YAML.
///
/// # Panics
///
/// As [`model_of`], plus a panic if the profile does not parse or does not
/// describe the manifest.
#[must_use]
pub fn model_with_profile(manifest_yaml: &str, profile_yaml: &str) -> Model {
    let manifest = Manifest::from_yaml_str(manifest_yaml).expect("fixture manifest must parse");
    manifest.validate().expect("fixture manifest must validate");
    let (graph, _) = DataflowGraph::from_manifest(&manifest).expect("fixture graph must build");
    let profile = Profile::from_yaml_str(profile_yaml).expect("fixture profile must parse");
    assert!(
        profile.validate_against(&graph).is_empty(),
        "fixture profile must describe the fixture manifest"
    );
    Model::from_graph_with_profile(&graph, &profile).expect("fixture model must build")
}
