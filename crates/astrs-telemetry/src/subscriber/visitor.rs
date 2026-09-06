//! [`FieldVisitor`] — the one `tracing::field::Visit` implementation
//! shared by [`crate::subscriber::fmt_layer::AstrsFmtLayer`] and
//! [`crate::subscriber::span_layer::AstrsSpanLayer`].

use std::collections::BTreeMap;

use tracing::field::{Field, Visit};

/// Collects a `tracing::Event`'s or span's fields into a JSON-valued map,
/// pulling the conventional `message` field out separately.
#[derive(Debug, Default)]
pub(crate) struct FieldVisitor {
    /// The `message` field's rendering, if the event/span had one.
    pub message: Option<String>,
    /// Every other field, keyed by name.
    pub fields: BTreeMap<String, serde_json::Value>,
}

impl FieldVisitor {
    /// Renders every field (excluding `message`) as a plain string map —
    /// [`astrs_wire::TraceSpan::attributes`]'s shape. A JSON string field
    /// renders bare; anything else (numbers, bools, nested structures)
    /// renders as compact JSON, so structure is never silently lost.
    pub(crate) fn attributes_as_strings(&self) -> BTreeMap<String, String> {
        self.fields
            .iter()
            .map(|(key, value)| (key.clone(), Self::value_to_string(value)))
            .collect()
    }

    fn value_to_string(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(rendered);
        } else {
            self.fields
                .insert(field.name().to_owned(), serde_json::Value::String(rendered));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        } else {
            self.fields.insert(
                field.name().to_owned(),
                serde_json::Value::String(value.to_owned()),
            );
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_owned(), serde_json::Value::Bool(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_owned(), serde_json::Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_owned(), serde_json::Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        let json_value = serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or_else(|| serde_json::Value::String(value.to_string()));
        self.fields.insert(field.name().to_owned(), json_value);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn attributes_as_strings_renders_plain_strings_bare() {
        let mut fields = BTreeMap::new();
        fields.insert(
            "node".to_owned(),
            serde_json::Value::String("camera".to_owned()),
        );
        let visitor = FieldVisitor {
            message: None,
            fields,
        };
        assert_eq!(visitor.attributes_as_strings()["node"], "camera");
    }

    #[test]
    fn attributes_as_strings_renders_non_strings_as_compact_json() {
        let mut fields = BTreeMap::new();
        fields.insert("count".to_owned(), serde_json::Value::from(3));
        fields.insert("ok".to_owned(), serde_json::Value::Bool(true));
        let visitor = FieldVisitor {
            message: None,
            fields,
        };
        let rendered = visitor.attributes_as_strings();
        assert_eq!(rendered["count"], "3");
        assert_eq!(rendered["ok"], "true");
    }
}
