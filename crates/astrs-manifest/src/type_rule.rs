//! Graph-wide implicit type coercion rules (blueprint §8.2 `type_rules`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Urn;

/// One entry of the manifest root's `type_rules: [{from, to}]` list.
///
/// Declares that an edge from an output typed `from` may feed an input
/// typed `to` without `strict_types` rejecting it. This crate stores and
/// syntax-checks the two URNs (see [`crate::Manifest::validate`]);
/// resolving what a type rule actually means for edge type-checking is
/// `astrs-graph`'s concern (§5.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TypeRule {
    /// The producer-side type URN this rule applies from.
    pub from: Urn,
    /// The consumer-side type URN this rule allows coercion to.
    pub to: Urn,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn parses_from_to() {
        let yaml = "from: std/core/v1/Float32\nto: std/core/v1/Float64\n";
        let rule: TypeRule = astrs_yaml::from_str(yaml).unwrap();
        assert_eq!(rule.from.as_str(), "std/core/v1/Float32");
        assert_eq!(rule.to.as_str(), "std/core/v1/Float64");
    }

    #[test]
    fn rejects_unknown_fields() {
        let yaml = "from: a/v1/B\nto: a/v1/C\nbogus: 1\n";
        assert!(astrs_yaml::from_str::<TypeRule>(yaml).is_err());
    }

    #[test]
    fn requires_both_fields() {
        assert!(astrs_yaml::from_str::<TypeRule>("from: a/v1/B\n").is_err());
        assert!(astrs_yaml::from_str::<TypeRule>("to: a/v1/B\n").is_err());
    }
}
