//! Shapes shared by the OTLP trace and metrics JSON payloads: `Resource`,
//! `InstrumentationScope`, `KeyValue`/`AnyValue`.
//!
//! These mirror the `opentelemetry.proto.common.v1` / `resource.v1`
//! messages' JSON mapping closely enough for any standard OTLP/HTTP
//! collector to accept them, without depending on the (banned,
//! §18.1) `opentelemetry-*`/`prost` crates: this is a hand-written,
//! serde-driven subset covering exactly what AstRS's data model can
//! produce.
//!
//! # Why `AnyValue` only ever holds a string
//!
//! The full OTLP `AnyValue` is a `oneof` over string/bool/int/double/
//! array/kvlist/bytes. Every attribute value this crate has to offer —
//! [`astrs_wire::TraceSpan::attributes`], [`astrs_wire::MetricPoint::labels`] —
//! is already a plain `String` by the time it reaches this module, so
//! only the `stringValue` arm is implemented. A future producer that
//! wants a richer OTLP value shape would extend this type rather than
//! route around it.

use serde::{Deserialize, Serialize};

/// One resource or span attribute: `{"key": "...", "value": {"stringValue": "..."}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyValue {
    /// The attribute key.
    pub key: String,
    /// The attribute value.
    pub value: AnyValue,
}

impl KeyValue {
    /// Builds a string-valued key/value pair.
    #[must_use]
    pub fn string(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: AnyValue::string(value),
        }
    }
}

/// An OTLP `AnyValue`, restricted to the `stringValue` arm — see the
/// module docs for why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnyValue {
    /// The string payload.
    #[serde(rename = "stringValue")]
    pub string_value: String,
}

impl AnyValue {
    /// Builds a string value.
    #[must_use]
    pub fn string(value: impl Into<String>) -> Self {
        Self {
            string_value: value.into(),
        }
    }
}

/// An OTLP `Resource`: the process/service this data describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Resource {
    /// Resource attributes, e.g. `service.name`, `service.instance.id`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<KeyValue>,
}

impl Resource {
    /// Builds a resource from `(key, value)` string pairs, in the given
    /// order (OTLP does not require any particular attribute order, but
    /// a fixed order keeps this crate's own golden-file tests stable).
    #[must_use]
    pub fn from_attributes<'a>(attrs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self {
            attributes: attrs
                .into_iter()
                .map(|(k, v)| KeyValue::string(k, v))
                .collect(),
        }
    }
}

/// An OTLP `InstrumentationScope`: which instrumentation produced the
/// data, e.g. `"astrs-telemetry"` at this crate's own version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    /// The scope name.
    pub name: String,
    /// The scope version, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl Scope {
    /// Builds a scope with a name and version.
    #[must_use]
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: Some(version.into()),
        }
    }
}

/// This crate's own instrumentation scope name, used as the default
/// `Scope::name` by [`crate::otlp::trace::build_trace_request`] and
/// [`crate::otlp::metrics::build_metrics_request`].
pub const SCOPE_NAME: &str = "astrs-telemetry";

/// This crate's compiled version, used as the default `Scope::version`.
pub const SCOPE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn key_value_serializes_as_a_string_any_value() {
        let kv = KeyValue::string("service.name", "astrs-daemon");
        let json = serde_json::to_string(&kv).unwrap();
        assert_eq!(
            json,
            r#"{"key":"service.name","value":{"stringValue":"astrs-daemon"}}"#
        );
    }

    #[test]
    fn resource_omits_empty_attributes() {
        let resource = Resource::default();
        let json = serde_json::to_string(&resource).unwrap();
        assert_eq!(json, "{}");
    }

    #[test]
    fn resource_from_attributes_preserves_order() {
        let resource = Resource::from_attributes([("a", "1"), ("b", "2")]);
        assert_eq!(resource.attributes[0].key, "a");
        assert_eq!(resource.attributes[1].key, "b");
    }

    #[test]
    fn scope_omits_absent_version() {
        let scope = Scope {
            name: "x".to_owned(),
            version: None,
        };
        assert_eq!(serde_json::to_string(&scope).unwrap(), r#"{"name":"x"}"#);
    }

    #[test]
    fn scope_new_sets_both_fields() {
        let scope = Scope::new("astrs-telemetry", "0.1.0");
        assert_eq!(
            serde_json::to_string(&scope).unwrap(),
            r#"{"name":"astrs-telemetry","version":"0.1.0"}"#
        );
    }
}
