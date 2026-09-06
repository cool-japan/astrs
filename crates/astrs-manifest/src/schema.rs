//! JSON Schema emission for the manifest format.
//!
//! [`emit_schema`] drives editor completion for `.astrs.yml` files and, via
//! `xtask schema`, the checked-in `astrs-schema.json` the blueprint
//! describes (§8.1's leading comment: "astrs-schema.json is generated from
//! the Rust structs"). Wiring `xtask schema` up to call this function is
//! the `xtask` crate's job, not this one's.

use crate::Manifest;

/// Render the [`Manifest`] JSON Schema (2020-12 dialect, schemars'
/// default) as a pretty-printed string.
///
/// This cannot practically fail — `Manifest`'s schema is built entirely
/// from derived `JsonSchema` impls over plain data, with no
/// `Serialize`-time computation involved — but on the off chance schema
/// serialization ever does fail, a minimal valid JSON error document is
/// returned instead of panicking, keeping this function panic-free like
/// every other public entry point in the crate.
#[must_use]
pub fn emit_schema() -> String {
    let schema = schemars::schema_for!(Manifest);
    serde_json::to_string_pretty(&schema)
        .unwrap_or_else(|err| format!("{{\"error\": \"schema serialization failed: {err}\"}}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn emits_parseable_json() {
        let schema = emit_schema();
        let value: serde_json::Value = serde_json::from_str(&schema).unwrap();
        assert!(value.is_object());
    }

    #[test]
    fn schema_documents_the_nodes_field() {
        let schema = emit_schema();
        assert!(schema.contains("\"nodes\""), "schema was: {schema}");
    }

    #[test]
    fn schema_never_mentions_output_framing() {
        // Blueprint §2.2: `output_framing` must not exist anywhere,
        // including in generated tooling artifacts.
        let schema = emit_schema();
        assert!(!schema.contains("output_framing"), "schema was: {schema}");
    }
}
