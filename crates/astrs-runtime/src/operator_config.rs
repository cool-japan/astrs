//! Converting a manifest operator's JSON `config:` map
//! ([`astrs_manifest::OperatorConfig::config`]) into the closed
//! [`astrs_wire::Parameter`] vocabulary
//! [`astrs_operator_api::Operator::configure`] receives (blueprint §9.3,
//! §6.1).
//!
//! `astrs-manifest` keeps `config:` as opaque `serde_json::Value` on
//! purpose (see that crate's `OperatorConfig` docs: "per-operator schemas
//! are out of this crate's scope"). [`astrs_operator_api::Operator::configure`],
//! on the other hand, receives the same closed, flat value set every other
//! piece of metadata in this system already uses
//! ([`Parameter`] — blueprint §6.1's `Bool | Integer | Float | String |
//! ListInt | ListFloat | ListString | Timestamp`), so an operator author
//! reads static construction-time configuration and live `astrs param set`
//! updates ([`astrs_operator_api::OpEvent::ParamUpdate`]) through one
//! shared vocabulary rather than two. This module is the one place JSON is
//! narrowed to that vocabulary, once, at [`crate::RuntimeHost::new`] time —
//! before any operator thread runs, so a manifest `config:` value with no
//! [`Parameter`] equivalent (a nested object, a `null`, a mixed-type
//! array) fails the whole host's construction with a clear per-operator,
//! per-key error, exactly like an unresolved input (`Routing::build`) does
//! — never half-way through a run.
//!
//! # Mapping rules
//!
//! | JSON | [`Parameter`] |
//! |---|---|
//! | `true` / `false` | [`Parameter::Bool`] |
//! | a number representable as `i64` | [`Parameter::Integer`] |
//! | any other JSON number | [`Parameter::Float`] |
//! | a string | [`Parameter::String`] |
//! | `[]` (empty array) | [`Parameter::ListString`] (`[]`) — an arbitrary but documented choice; an empty array carries no element to infer a richer element type from |
//! | an array of only strings | [`Parameter::ListString`] |
//! | an array of only integer-representable numbers | [`Parameter::ListInt`] |
//! | an array of only numbers (otherwise) | [`Parameter::ListFloat`] |
//! | `null`, an object, or a mixed-type array | [`ConfigValueError`] (no equivalent) |
//!
//! `null` and nested objects have no equivalent because [`Parameter`] is
//! deliberately not a general JSON value (blueprint §6.1: "a recursive
//! value type would make the hot path pay for arbitrary nesting nobody
//! needs" — the same reasoning applies to per-instance static
//! configuration as to per-message metadata). A manifest author who needs
//! richer per-operator configuration than this flat vocabulary allows is
//! expected to carry it in the payload, not in `config:`.

use std::collections::BTreeMap;

use astrs_manifest::OperatorConfig;
use astrs_wire::Parameter;
use serde_json::Value;

use crate::error::RuntimeError;

/// Why a manifest operator's `config:` value has no equivalent in the
/// closed [`Parameter`] vocabulary.
///
/// `#[non_exhaustive]`: the append-only evolution rule (blueprint §3.4)
/// applies here too — a future [`Parameter`] variant could shrink this set
/// without breaking callers who only match a wildcard arm today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigValueError {
    /// A JSON `null`.
    #[error("null has no equivalent in the closed parameter value set")]
    Null,
    /// A JSON object (map). [`Parameter`] is flat — no nesting.
    #[error("a nested object has no equivalent in the closed parameter value set")]
    NestedObject,
    /// A JSON array whose elements are not all the same scalar kind (all
    /// strings, all integer-representable numbers, or all numbers).
    #[error("array elements are not all the same scalar type (integer, float, or string)")]
    MixedArray,
    /// A JSON number [`serde_json::Number`] itself cannot represent as
    /// either `i64` or `f64` — unreachable for ordinary JSON input, kept
    /// only because this module's conversion is written as total over
    /// every `serde_json::Number` this build might ever construct, not
    /// just the ones a well-formed manifest produces.
    #[error("number is not representable as either an i64 or an f64")]
    UnrepresentableNumber,
}

/// Converts one JSON value to the [`Parameter`] it denotes.
///
/// # Errors
///
/// See [`ConfigValueError`] and this module's mapping table.
fn json_to_parameter(value: &Value) -> Result<Parameter, ConfigValueError> {
    match value {
        Value::Null => Err(ConfigValueError::Null),
        Value::Bool(flag) => Ok(Parameter::Bool(*flag)),
        Value::Number(number) => number
            .as_i64()
            .map(Parameter::Integer)
            .or_else(|| number.as_f64().map(Parameter::Float))
            .ok_or(ConfigValueError::UnrepresentableNumber),
        Value::String(text) => Ok(Parameter::String(text.clone())),
        Value::Array(items) => array_to_parameter(items),
        Value::Object(_) => Err(ConfigValueError::NestedObject),
    }
}

/// The [`Parameter`] list variant `items` denotes, by the rules in this
/// module's docs.
fn array_to_parameter(items: &[Value]) -> Result<Parameter, ConfigValueError> {
    if items.is_empty() {
        return Ok(Parameter::ListString(Vec::new()));
    }
    if items.iter().all(Value::is_string) {
        return Ok(Parameter::ListString(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        ));
    }
    if items
        .iter()
        .all(|item| matches!(item, Value::Number(number) if number.as_i64().is_some()))
    {
        return Ok(Parameter::ListInt(
            items.iter().filter_map(Value::as_i64).collect(),
        ));
    }
    if items.iter().all(Value::is_number) {
        return Ok(Parameter::ListFloat(
            items.iter().filter_map(Value::as_f64).collect(),
        ));
    }
    Err(ConfigValueError::MixedArray)
}

/// Converts every hosted operator's manifest `config:` map to the
/// [`Parameter`] vocabulary [`astrs_operator_api::Operator::configure`]
/// receives, in `operators`' own order.
///
/// # Errors
///
/// [`RuntimeError::InvalidConfigValue`] naming the first operator and key
/// whose value has no [`Parameter`] equivalent.
pub(crate) fn resolve_configs(
    operators: &[OperatorConfig],
) -> Result<Vec<BTreeMap<String, Parameter>>, RuntimeError> {
    operators
        .iter()
        .map(|op| {
            op.config
                .iter()
                .map(|(key, value)| {
                    json_to_parameter(value)
                        .map(|parameter| (key.clone(), parameter))
                        .map_err(|source| RuntimeError::invalid_config_value(&op.id, key, source))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use serde_json::json;

    #[test]
    fn scalars_convert_directly() {
        assert_eq!(
            json_to_parameter(&json!(true)).unwrap(),
            Parameter::Bool(true)
        );
        assert_eq!(json_to_parameter(&json!(7)).unwrap(), Parameter::Integer(7));
        assert_eq!(
            json_to_parameter(&json!(-3)).unwrap(),
            Parameter::Integer(-3)
        );
        assert_eq!(
            json_to_parameter(&json!(1.5)).unwrap(),
            Parameter::Float(1.5)
        );
        assert_eq!(
            json_to_parameter(&json!("hi")).unwrap(),
            Parameter::String("hi".to_owned())
        );
    }

    #[test]
    fn a_decimal_literal_stays_a_float_even_with_no_fractional_part() {
        // `serde_json` tags a `Number` by how it was *written*, not by
        // whether its value happens to be integral: `7.0`'s `as_i64()`
        // returns `None` (verified against `serde_json` directly, not
        // assumed) precisely because a decimal point was present in the
        // source, so this module's `.as_i64().or_else(.as_f64())` chain
        // reaches the float branch for it, same as it would for `7.5`.
        // Only a literal written with no decimal point or exponent at all
        // (`json!(7)`) takes the integer branch — see `scalars_convert_directly`.
        assert_eq!(
            json_to_parameter(&json!(7.0)).unwrap(),
            Parameter::Float(7.0)
        );
    }

    #[test]
    fn homogeneous_arrays_convert_to_the_matching_list_variant() {
        assert_eq!(
            json_to_parameter(&json!(["a", "b"])).unwrap(),
            Parameter::ListString(vec!["a".to_owned(), "b".to_owned()])
        );
        assert_eq!(
            json_to_parameter(&json!([1, 2, 3])).unwrap(),
            Parameter::ListInt(vec![1, 2, 3])
        );
        assert_eq!(
            json_to_parameter(&json!([1, 2.5])).unwrap(),
            Parameter::ListFloat(vec![1.0, 2.5]),
            "a mix of integer- and float-shaped numbers is a float list"
        );
    }

    #[test]
    fn an_empty_array_is_an_empty_string_list() {
        assert_eq!(
            json_to_parameter(&json!([])).unwrap(),
            Parameter::ListString(vec![])
        );
    }

    #[test]
    fn null_and_nested_objects_have_no_equivalent() {
        assert_eq!(
            json_to_parameter(&json!(null)).unwrap_err(),
            ConfigValueError::Null
        );
        assert_eq!(
            json_to_parameter(&json!({"nested": true})).unwrap_err(),
            ConfigValueError::NestedObject
        );
    }

    #[test]
    fn a_mixed_type_array_is_rejected() {
        assert_eq!(
            json_to_parameter(&json!([1, "two"])).unwrap_err(),
            ConfigValueError::MixedArray
        );
    }

    fn operator(id: &str, config: BTreeMap<String, Value>) -> OperatorConfig {
        OperatorConfig {
            id: id.to_owned(),
            operator: format!("{id}Type"),
            dylib: None,
            wasm: None,
            hub: None,
            inputs: BTreeMap::new(),
            outputs: Vec::new(),
            config,
        }
    }

    #[test]
    fn resolve_configs_converts_every_operator_in_order() {
        let mut a_config = BTreeMap::new();
        a_config.insert("threshold".to_owned(), json!(7));
        let mut b_config = BTreeMap::new();
        b_config.insert("labels".to_owned(), json!(["person", "car"]));

        let operators = vec![operator("a", a_config), operator("b", b_config)];

        let resolved = resolve_configs(&operators).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].get("threshold"), Some(&Parameter::Integer(7)));
        assert_eq!(
            resolved[1].get("labels"),
            Some(&Parameter::ListString(vec![
                "person".to_owned(),
                "car".to_owned()
            ]))
        );
    }

    #[test]
    fn resolve_configs_reports_the_offending_operator_and_key() {
        let mut config = BTreeMap::new();
        config.insert("bad".to_owned(), json!(null));
        let operators = vec![operator("crop", config)];

        let error = resolve_configs(&operators).unwrap_err();
        match error {
            RuntimeError::InvalidConfigValue {
                operator,
                key,
                source,
            } => {
                assert_eq!(operator, "crop");
                assert_eq!(key, "bad");
                assert_eq!(source, ConfigValueError::Null);
            }
            other => panic!("expected InvalidConfigValue, got {other}"),
        }
    }

    #[test]
    fn an_operator_with_no_config_resolves_to_an_empty_map() {
        let operators = vec![operator("noop", BTreeMap::new())];
        let resolved = resolve_configs(&operators).unwrap();
        assert_eq!(resolved, vec![BTreeMap::new()]);
    }
}
