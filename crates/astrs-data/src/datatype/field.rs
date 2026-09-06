//! [`Field`] and [`Schema`] — the names and nullability that turn a bag of
//! buffers into a self-describing payload.
//!
//! A [`Field`] is exactly `{name, data_type, nullable}`, matching the blueprint
//! §6.1 model. Arrow's optional per-field metadata map is intentionally *not*
//! modelled: AstRS carries metadata at the schema level (where `_schema_hash`
//! and the type URN live) and beside the payload in the oxicode-encoded
//! metadata struct, so a second per-field map would be a third place to look.
//!
//! ```
//! use astrs_data::{DataType, Field, Schema};
//!
//! let schema = Schema::new(vec![Field::new("data", DataType::Float32, false)])
//!     .with_metadata_entry("std/type", "std/core/v1/Float32");
//!
//! assert!(!schema.field(0).map(Field::is_nullable).unwrap_or(true));
//! assert_eq!(schema.metadata_value("std/type"), Some("std/core/v1/Float32"));
//! ```

use std::collections::BTreeMap;
use std::fmt;

use crate::datatype::DataType;
use crate::error::{DataError, Result};

/// One named column in a [`Schema`] or one child of a nested [`DataType`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Field {
    /// Column name. Not required to be unique inside a nested type, but
    /// [`Schema::try_new`] rejects duplicates at the top level.
    name: String,
    /// Layout of the column's values.
    data_type: DataType,
    /// Whether the column may contain nulls. A non-nullable column must have
    /// `null_count == 0`; [`crate::RecordBatch::try_new`] enforces it.
    nullable: bool,
}

impl Field {
    /// Creates a field.
    ///
    /// ```
    /// use astrs_data::{DataType, Field};
    ///
    /// let field = Field::new("stamp", DataType::Timestamp, false);
    /// assert_eq!(field.name(), "stamp");
    /// assert!(!field.is_nullable());
    /// ```
    #[must_use]
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable,
        }
    }

    /// Creates a nullable field. The common case for payload columns.
    #[must_use]
    pub fn nullable(name: impl Into<String>, data_type: DataType) -> Self {
        Self::new(name, data_type, true)
    }

    /// Creates a non-nullable field.
    #[must_use]
    pub fn required(name: impl Into<String>, data_type: DataType) -> Self {
        Self::new(name, data_type, false)
    }

    /// The column name.
    #[inline]
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The column's layout.
    #[inline]
    #[must_use]
    pub const fn data_type(&self) -> &DataType {
        &self.data_type
    }

    /// Whether the column may contain nulls.
    #[inline]
    #[must_use]
    pub const fn is_nullable(&self) -> bool {
        self.nullable
    }

    /// Returns a copy with a different name.
    #[must_use]
    pub fn with_name(&self, name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            data_type: self.data_type.clone(),
            nullable: self.nullable,
        }
    }

    /// Returns a copy with a different nullability.
    #[must_use]
    pub fn with_nullable(&self, nullable: bool) -> Self {
        Self {
            name: self.name.clone(),
            data_type: self.data_type.clone(),
            nullable,
        }
    }

    /// Returns a copy with a different data type.
    #[must_use]
    pub fn with_data_type(&self, data_type: DataType) -> Self {
        Self {
            name: self.name.clone(),
            data_type,
            nullable: self.nullable,
        }
    }
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.name, self.data_type)?;
        if self.nullable {
            f.write_str("?")?;
        }
        Ok(())
    }
}

/// An ordered list of [`Field`]s plus a string metadata map.
///
/// Schemas are compared by value and hashed structurally, which is what lets a
/// receiver cache decoded schemas and detect type drift (blueprint §6.1). The
/// metadata map is a [`BTreeMap`] so the ordering — and therefore the hash and
/// the encoded bytes — is deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Schema {
    /// Columns, in order.
    fields: Vec<Field>,
    /// Key/value metadata. Keys starting with `_` are AstRS-internal
    /// (`_schema_hash`) and are stripped before user delivery.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    metadata: BTreeMap<String, String>,
}

impl Schema {
    /// Creates a schema with no metadata.
    ///
    /// Duplicate field names are *not* rejected here; use
    /// [`Schema::try_new`] when the input is untrusted.
    #[must_use]
    pub fn new(fields: impl IntoIterator<Item = Field>) -> Self {
        Self {
            fields: fields.into_iter().collect(),
            metadata: BTreeMap::new(),
        }
    }

    /// Creates a schema, rejecting duplicate field names.
    ///
    /// # Errors
    ///
    /// [`DataError::DuplicateFieldName`] when two fields share a name.
    ///
    /// ```
    /// use astrs_data::{DataType, Field, Schema};
    ///
    /// assert!(Schema::try_new(vec![
    ///     Field::new("a", DataType::Int8, true),
    ///     Field::new("b", DataType::Int8, true),
    /// ]).is_ok());
    /// assert!(Schema::try_new(vec![
    ///     Field::new("a", DataType::Int8, true),
    ///     Field::new("a", DataType::Int16, true),
    /// ]).is_err());
    /// ```
    pub fn try_new(fields: impl IntoIterator<Item = Field>) -> Result<Self> {
        let fields: Vec<Field> = fields.into_iter().collect();
        let mut seen = std::collections::BTreeSet::new();
        for field in &fields {
            if !seen.insert(field.name()) {
                return Err(DataError::DuplicateFieldName {
                    name: field.name().to_owned(),
                });
            }
        }
        Ok(Self {
            fields,
            metadata: BTreeMap::new(),
        })
    }

    /// The single-column schema AstRS payloads use (blueprint §6.1).
    ///
    /// ```
    /// use astrs_data::{DataType, Schema, DATA_COLUMN};
    ///
    /// let schema = Schema::payload(DataType::Float32, false);
    /// assert_eq!(schema.field(0).map(|f| f.name()), Some(DATA_COLUMN));
    /// ```
    #[must_use]
    pub fn payload(data_type: DataType, nullable: bool) -> Self {
        Self::new(vec![Field::new(crate::DATA_COLUMN, data_type, nullable)])
    }

    /// Returns a copy carrying `metadata`.
    #[must_use]
    pub fn with_metadata(mut self, metadata: BTreeMap<String, String>) -> Self {
        self.metadata = metadata;
        self
    }

    /// Returns a copy with one metadata entry added or replaced.
    #[must_use]
    pub fn with_metadata_entry(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// The columns, in order.
    #[inline]
    #[must_use]
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// The metadata map.
    #[inline]
    #[must_use]
    pub const fn metadata(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }

    /// The metadata map, mutably.
    #[inline]
    pub const fn metadata_mut(&mut self) -> &mut BTreeMap<String, String> {
        &mut self.metadata
    }

    /// One metadata value by key.
    #[inline]
    #[must_use]
    pub fn metadata_value(&self, key: &str) -> Option<&str> {
        self.metadata.get(key).map(String::as_str)
    }

    /// Number of columns.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.fields.len()
    }

    /// Returns `true` when the schema has no columns.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// The field at `index`.
    #[inline]
    #[must_use]
    pub fn field(&self, index: usize) -> Option<&Field> {
        self.fields.get(index)
    }

    /// The first field named `name`.
    #[must_use]
    pub fn field_by_name(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|field| field.name() == name)
    }

    /// The position of the first field named `name`.
    #[must_use]
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|field| field.name() == name)
    }

    /// The position of the first field named `name`.
    ///
    /// # Errors
    ///
    /// [`DataError::FieldNotFound`] when no field has that name.
    pub fn try_index_of(&self, name: &str) -> Result<usize> {
        self.index_of(name).ok_or_else(|| DataError::FieldNotFound {
            name: name.to_owned(),
        })
    }

    /// Iterates over the fields.
    pub fn iter(&self) -> std::slice::Iter<'_, Field> {
        self.fields.iter()
    }

    /// A schema holding only the fields at `indices`, in the order given.
    ///
    /// # Errors
    ///
    /// [`DataError::IndexOutOfBounds`] when an index has no field.
    ///
    /// ```
    /// use astrs_data::{DataType, Field, Schema};
    ///
    /// let schema = Schema::new(vec![
    ///     Field::new("a", DataType::Int8, true),
    ///     Field::new("b", DataType::Int16, true),
    ///     Field::new("c", DataType::Int32, true),
    /// ]);
    /// let projected = schema.project(&[2, 0])?;
    /// assert_eq!(projected.field(0).map(|f| f.name()), Some("c"));
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn project(&self, indices: &[usize]) -> Result<Self> {
        let mut fields = Vec::with_capacity(indices.len());
        for &index in indices {
            let field = self.fields.get(index).ok_or(DataError::IndexOutOfBounds {
                index,
                len: self.fields.len(),
            })?;
            fields.push(field.clone());
        }
        Ok(Self {
            fields,
            metadata: self.metadata.clone(),
        })
    }

    /// Returns `true` when the two schemas describe the same layout, ignoring
    /// metadata, field names and nullability.
    ///
    /// This is the compatibility check the graph type system runs on an edge:
    /// producer and consumer must agree on *bytes*, not on labels.
    #[must_use]
    pub fn layout_eq(&self, other: &Self) -> bool {
        self.fields.len() == other.fields.len()
            && self
                .fields
                .iter()
                .zip(other.fields.iter())
                .all(|(a, b)| a.data_type().layout_eq(b.data_type()))
    }
}

impl fmt::Display for Schema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Schema{")?;
        for (index, field) in self.fields.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{field}")?;
        }
        f.write_str("}")
    }
}

impl<'a> IntoIterator for &'a Schema {
    type Item = &'a Field;
    type IntoIter = std::slice::Iter<'a, Field>;

    fn into_iter(self) -> Self::IntoIter {
        self.fields.iter()
    }
}

impl FromIterator<Field> for Schema {
    fn from_iter<I: IntoIterator<Item = Field>>(iter: I) -> Self {
        Self::new(iter)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn sample() -> Schema {
        Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("stamp", DataType::Timestamp, false),
        ])
    }

    #[test]
    fn field_accessors() {
        let field = Field::new("stamp", DataType::Timestamp, false);
        assert_eq!(field.name(), "stamp");
        assert_eq!(field.data_type(), &DataType::Timestamp);
        assert!(!field.is_nullable());
        assert!(Field::nullable("x", DataType::Int8).is_nullable());
        assert!(!Field::required("x", DataType::Int8).is_nullable());
    }

    #[test]
    fn field_builders_return_copies() {
        let field = Field::new("a", DataType::Int8, false);
        assert_eq!(field.with_name("b").name(), "b");
        assert!(field.with_nullable(true).is_nullable());
        assert_eq!(
            field.with_data_type(DataType::Utf8).data_type(),
            &DataType::Utf8
        );
        assert_eq!(field.name(), "a", "the original is untouched");
    }

    #[test]
    fn field_display_marks_nullability() {
        assert_eq!(Field::required("a", DataType::Int8).to_string(), "a: Int8");
        assert_eq!(Field::nullable("a", DataType::Int8).to_string(), "a: Int8?");
    }

    #[test]
    fn schema_lookup() {
        let schema = sample();
        assert_eq!(schema.len(), 3);
        assert!(!schema.is_empty());
        assert_eq!(schema.field(1).map(Field::name), Some("name"));
        assert_eq!(schema.field(9), None);
        assert_eq!(
            schema.field_by_name("stamp").map(Field::name),
            Some("stamp")
        );
        assert_eq!(schema.field_by_name("missing"), None);
        assert_eq!(schema.index_of("name"), Some(1));
        assert_eq!(schema.index_of("missing"), None);
        assert_eq!(schema.try_index_of("id").unwrap(), 0);
        assert_eq!(
            schema.try_index_of("nope").unwrap_err(),
            DataError::FieldNotFound {
                name: "nope".to_owned()
            }
        );
    }

    #[test]
    fn empty_schema() {
        let schema = Schema::default();
        assert!(schema.is_empty());
        assert_eq!(schema.len(), 0);
        assert_eq!(schema.iter().count(), 0);
        assert_eq!(schema.to_string(), "Schema{}");
    }

    #[test]
    fn try_new_rejects_duplicate_names() {
        assert!(Schema::try_new(sample().fields().to_vec()).is_ok());
        let err = Schema::try_new(vec![
            Field::new("a", DataType::Int8, true),
            Field::new("a", DataType::Int16, true),
        ])
        .unwrap_err();
        assert_eq!(
            err,
            DataError::DuplicateFieldName {
                name: "a".to_owned()
            }
        );
    }

    #[test]
    fn metadata_is_ordered_and_addressable() {
        let mut schema = sample()
            .with_metadata_entry("z", "last")
            .with_metadata_entry("a", "first");
        assert_eq!(schema.metadata_value("a"), Some("first"));
        assert_eq!(schema.metadata_value("missing"), None);
        assert_eq!(
            schema.metadata().keys().collect::<Vec<_>>(),
            vec!["a", "z"],
            "BTreeMap keeps a deterministic order"
        );
        schema.metadata_mut().insert("m".into(), "mid".into());
        assert_eq!(schema.metadata().len(), 3);

        let replaced = schema.clone().with_metadata(BTreeMap::new());
        assert!(replaced.metadata().is_empty());
    }

    #[test]
    fn payload_schema_uses_the_data_column() {
        let schema = Schema::payload(DataType::Float32, true);
        assert_eq!(schema.len(), 1);
        assert_eq!(schema.field(0).map(Field::name), Some(crate::DATA_COLUMN));
        assert!(schema.field(0).map(Field::is_nullable).unwrap_or(false));
    }

    #[test]
    fn projection_reorders_and_validates() {
        let schema = sample();
        let projected = schema.project(&[2, 0]).unwrap();
        assert_eq!(projected.len(), 2);
        assert_eq!(projected.field(0).map(Field::name), Some("stamp"));
        assert_eq!(projected.field(1).map(Field::name), Some("id"));
        assert_eq!(schema.project(&[]).unwrap().len(), 0);
        assert_eq!(
            schema.project(&[0, 7]).unwrap_err(),
            DataError::IndexOutOfBounds { index: 7, len: 3 }
        );
    }

    #[test]
    fn projection_keeps_metadata() {
        let schema = sample().with_metadata_entry("k", "v");
        let projected = schema.project(&[0]).unwrap();
        assert_eq!(projected.metadata_value("k"), Some("v"));
    }

    #[test]
    fn layout_eq_ignores_labels_and_metadata() {
        let a =
            Schema::new(vec![Field::new("a", DataType::Int32, true)]).with_metadata_entry("x", "1");
        let b = Schema::new(vec![Field::new("b", DataType::Int32, false)]);
        assert_ne!(a, b);
        assert!(a.layout_eq(&b));
        assert!(!a.layout_eq(&Schema::new(vec![Field::new("a", DataType::Int64, true)])));
        assert!(!a.layout_eq(&Schema::default()));
    }

    #[test]
    fn equality_includes_metadata() {
        let a = sample().with_metadata_entry("k", "v");
        let b = sample().with_metadata_entry("k", "w");
        assert_ne!(a, b);
        assert_eq!(a, sample().with_metadata_entry("k", "v"));
    }

    #[test]
    fn hashing_is_structural() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let hash = |schema: &Schema| {
            let mut hasher = DefaultHasher::new();
            schema.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash(&sample()), hash(&sample()));
        assert_ne!(
            hash(&sample()),
            hash(&sample().with_metadata_entry("k", "v"))
        );
    }

    #[test]
    fn display_and_iteration() {
        let schema = sample();
        assert_eq!(
            schema.to_string(),
            "Schema{id: Int32, name: Utf8?, stamp: Timestamp(ns)}"
        );
        assert_eq!(schema.iter().count(), 3);
        assert_eq!((&schema).into_iter().count(), 3);
        let collected: Schema = schema.fields().iter().cloned().collect();
        assert_eq!(collected.len(), 3);
    }

    #[test]
    fn serde_round_trip() {
        let schema = sample().with_metadata_entry("_schema_hash", "deadbeef");
        let json = serde_json::to_string(&schema).unwrap();
        assert_eq!(serde_json::from_str::<Schema>(&json).unwrap(), schema);

        // Metadata is omitted when empty, so the common payload schema stays
        // compact on the wire.
        let bare = serde_json::to_string(&Schema::payload(DataType::Int8, false)).unwrap();
        assert!(!bare.contains("metadata"), "{bare}");
        assert!(serde_json::from_str::<Schema>(&bare).is_ok());
    }
}
