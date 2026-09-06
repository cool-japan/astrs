//! [`TypeUrnError`] — every way a type URN can be rejected.
//!
//! The variants split into three groups, which matters because callers react
//! to them differently:
//!
//! 1. **Syntax** ([`TypeUrnError::Empty`] … [`TypeUrnError::TooManyParams`]) —
//!    the text is not a URN at all. A manifest carrying one of these is
//!    malformed and `astrs validate` rejects it outright.
//! 2. **Resolution** ([`TypeUrnError::UnknownType`],
//!    [`TypeUrnError::UnknownParameter`], [`TypeUrnError::MissingParameter`],
//!    [`TypeUrnError::UnsupportedParameterValue`]) — the URN parses but the
//!    registry does not recognise it. A graph may still run with such a port
//!    when the edge is declared `type: any`.
//! 3. **Staging** ([`TypeUrnError::LayoutUnavailable`]) — the type is
//!    registered and its parameters are valid, but its normative columnar
//!    layout is not mapped in this build. This is the only variant that is
//!    expected to disappear as the crate is completed, and it is deliberately
//!    distinct from `UnknownType` so a caller can tell "you asked for
//!    something that does not exist" from "that exists, we cannot lay it out
//!    yet".
//!
//! Every variant owns its strings. URNs are short (capped at
//! [`TypeUrn::MAX_LEN`](crate::urn::TypeUrn::MAX_LEN)) and errors are rare, so
//! the allocation buys a diagnostic that survives the borrow.

/// A type URN could not be parsed or resolved.
///
/// `Clone + PartialEq + Eq` so tests and the conformance zoo can assert on an
/// exact failure rather than on a substring of its message.
///
/// ```
/// use astrs_data::urn::{TypeUrn, TypeUrnError};
///
/// assert_eq!(TypeUrn::parse(""), Err(TypeUrnError::Empty));
/// assert_eq!(
///     TypeUrn::parse("std/core/v1/Bool").map(|u| u.name().to_owned()),
///     Ok("Bool".to_owned())
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TypeUrnError {
    /// The text was empty, or contained nothing but whitespace.
    #[error("type URN is empty")]
    Empty,

    /// The text is longer than [`TypeUrn::MAX_LEN`](crate::urn::TypeUrn::MAX_LEN).
    ///
    /// Checked before any other work, because URNs arrive from manifests and
    /// from the wire, and neither is trusted to be short.
    #[error("type URN is {len} bytes, over the {max}-byte limit")]
    TooLong {
        /// Length of the rejected text, in bytes.
        len: usize,
        /// The limit.
        max: usize,
    },

    /// The text does not have exactly four `/`-separated segments.
    #[error("type URN needs 4 '/'-separated segments, found {found}")]
    SegmentCount {
        /// Number of segments the text actually has.
        found: usize,
    },

    /// The leading segment is not `std`.
    ///
    /// The namespace is a closed set of exactly one entry in 0.1.0. Opening
    /// it to vendor namespaces is the designed extension point: the parser
    /// gains a namespace table, nothing else in this module changes.
    #[error("unsupported type URN namespace {namespace:?}, expected \"std\"")]
    UnsupportedNamespace {
        /// The namespace that was found.
        namespace: String,
    },

    /// The category segment is not a lowercase identifier.
    #[error("invalid type URN category {category:?}")]
    InvalidCategory {
        /// The rejected segment.
        category: String,
    },

    /// The version segment is not `v` followed by a `u16` in canonical form.
    #[error("invalid type URN version {version:?}, expected \"v<0..=65535>\"")]
    InvalidVersion {
        /// The rejected segment.
        version: String,
    },

    /// The type-name segment is not an upper-camel identifier.
    #[error("invalid type URN name {name:?}")]
    InvalidName {
        /// The rejected segment.
        name: String,
    },

    /// A `[` was opened and never closed.
    #[error("type URN parameter list is not terminated with ']'")]
    UnterminatedParams,

    /// Something followed the closing `]`.
    #[error("unexpected trailing text {rest:?} after the type URN parameter list")]
    TrailingData {
        /// The text after `]`.
        rest: String,
    },

    /// The parameter list was written but is empty (`Type[]`).
    ///
    /// A type with no parameters is spelled without brackets, so that the
    /// canonical form of a URN is unique.
    #[error("type URN parameter list is empty; omit the brackets instead")]
    EmptyParams,

    /// A parameter has no `=`.
    #[error("type URN parameter {param:?} is not a \"key=value\" pair")]
    MalformedParam {
        /// The rejected entry.
        param: String,
    },

    /// A parameter's key is empty (`[=v]`).
    #[error("type URN parameter key is empty")]
    EmptyParamKey,

    /// A parameter's value is empty (`[k=]`).
    #[error("type URN parameter {key:?} has an empty value")]
    EmptyParamValue {
        /// The key whose value was missing.
        key: String,
    },

    /// A parameter's key is not a lowercase identifier.
    #[error("invalid type URN parameter key {key:?}")]
    InvalidParamKey {
        /// The rejected key.
        key: String,
    },

    /// A parameter's value contains a byte the grammar reserves.
    ///
    /// Values are `[A-Za-z0-9_.:+*-]+`. There is no escape syntax: a
    /// multi-valued parameter separates its elements with `:`
    /// (`fields=x:y:z`), since `,` already separates parameters.
    #[error("invalid value {value:?} for type URN parameter {key:?}")]
    InvalidParamValue {
        /// The key the value belongs to.
        key: String,
        /// The rejected value.
        value: String,
    },

    /// The same parameter key appeared twice.
    #[error("duplicate type URN parameter {key:?}")]
    DuplicateParam {
        /// The repeated key.
        key: String,
    },

    /// The parameter list is longer than [`TypeUrn::MAX_PARAMS`](crate::urn::TypeUrn::MAX_PARAMS).
    #[error("type URN carries {found} parameters, over the limit of {max}")]
    TooManyParams {
        /// Number of parameters supplied.
        found: usize,
        /// The limit.
        max: usize,
    },

    /// The registry has no entry for this type.
    #[error("unknown type URN {urn}")]
    UnknownType {
        /// The URN that was looked up, in canonical form.
        urn: String,
    },

    /// The type exists but does not accept this parameter.
    #[error("type {urn} does not accept the parameter {key:?}")]
    UnknownParameter {
        /// The type's base URN.
        urn: String,
        /// The parameter that is not in the type's accepted set.
        key: String,
    },

    /// The type requires a parameter that was not supplied.
    #[error("type {urn} requires the parameter {key:?}")]
    MissingParameter {
        /// The type's base URN.
        urn: String,
        /// The parameter that is required.
        key: String,
    },

    /// The parameter is accepted but this particular value is not.
    ///
    /// Raised by the parameterised layout resolvers — an `Image[pixel=???]`
    /// whose pixel format has no columnar mapping, for instance.
    #[error("type {urn} does not support {key}={value:?}")]
    UnsupportedParameterValue {
        /// The type's base URN.
        urn: String,
        /// The offending parameter key.
        key: String,
        /// The offending value.
        value: String,
    },

    /// The type is registered, but its columnar layout is not mapped in this
    /// build.
    ///
    /// Not a parse error and not an unknown type: the URN is valid and the
    /// registry knows it. Callers that only need to *compare* port types work
    /// fine; only callers that need the concrete [`DataType`](crate::DataType)
    /// are blocked.
    #[error("the columnar layout of {urn} is not registered in this build")]
    LayoutUnavailable {
        /// The type's base URN.
        urn: String,
    },

    /// A type was registered twice under the same base URN.
    #[error("type {urn} is already registered")]
    DuplicateRegistration {
        /// The base URN that collided.
        urn: String,
    },
}

impl TypeUrnError {
    /// Returns `true` for the syntax group: the text is not a URN.
    ///
    /// ```
    /// use astrs_data::urn::{TypeUrn, TypeUrnError};
    ///
    /// let err = TypeUrn::parse("std/core/v1/lowercase").unwrap_err();
    /// assert!(err.is_syntax());
    /// assert!(!err.is_resolution());
    /// ```
    #[must_use]
    pub const fn is_syntax(&self) -> bool {
        matches!(
            self,
            Self::Empty
                | Self::TooLong { .. }
                | Self::SegmentCount { .. }
                | Self::UnsupportedNamespace { .. }
                | Self::InvalidCategory { .. }
                | Self::InvalidVersion { .. }
                | Self::InvalidName { .. }
                | Self::UnterminatedParams
                | Self::TrailingData { .. }
                | Self::EmptyParams
                | Self::MalformedParam { .. }
                | Self::EmptyParamKey
                | Self::EmptyParamValue { .. }
                | Self::InvalidParamKey { .. }
                | Self::InvalidParamValue { .. }
                | Self::DuplicateParam { .. }
                | Self::TooManyParams { .. }
        )
    }

    /// Returns `true` for the resolution group: the URN parses, the registry
    /// disagrees with it.
    #[must_use]
    pub const fn is_resolution(&self) -> bool {
        matches!(
            self,
            Self::UnknownType { .. }
                | Self::UnknownParameter { .. }
                | Self::MissingParameter { .. }
                | Self::UnsupportedParameterValue { .. }
        )
    }

    /// Returns `true` when the failure is only that this build has not mapped
    /// the type's layout yet.
    ///
    /// Every `std/v1` type resolves in this build (see
    /// [`crate::urn::layouts`]), so this is demonstrated on a directly
    /// constructed error rather than through a registry lookup — the
    /// variant stays available for whatever a future, append-only revision
    /// registers before its layout lands.
    ///
    /// ```
    /// use astrs_data::urn::TypeUrnError;
    ///
    /// let err = TypeUrnError::LayoutUnavailable { urn: "std/future/v1/Ghost".to_owned() };
    /// assert!(err.is_layout_unavailable());
    /// ```
    #[must_use]
    pub const fn is_layout_unavailable(&self) -> bool {
        matches!(self, Self::LayoutUnavailable { .. })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn groups_partition_the_enum() {
        let syntax = TypeUrnError::Empty;
        let resolution = TypeUrnError::UnknownType {
            urn: "std/core/v1/Nope".to_owned(),
        };
        let staging = TypeUrnError::LayoutUnavailable {
            urn: "std/nav/v1/Path".to_owned(),
        };

        assert!(syntax.is_syntax() && !syntax.is_resolution() && !syntax.is_layout_unavailable());
        assert!(
            resolution.is_resolution()
                && !resolution.is_syntax()
                && !resolution.is_layout_unavailable()
        );
        assert!(
            staging.is_layout_unavailable() && !staging.is_syntax() && !staging.is_resolution()
        );
    }

    #[test]
    fn messages_name_the_offender() {
        let err = TypeUrnError::InvalidParamValue {
            key: "pixel".to_owned(),
            value: "r g b".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "invalid value \"r g b\" for type URN parameter \"pixel\""
        );

        let err = TypeUrnError::TooLong { len: 900, max: 512 };
        assert_eq!(
            err.to_string(),
            "type URN is 900 bytes, over the 512-byte limit"
        );
    }

    #[test]
    fn errors_compare_by_value() {
        assert_eq!(TypeUrnError::Empty, TypeUrnError::Empty);
        assert_ne!(
            TypeUrnError::EmptyParamValue {
                key: "a".to_owned()
            },
            TypeUrnError::EmptyParamValue {
                key: "b".to_owned()
            }
        );
    }

    #[test]
    fn flattens_into_the_crate_error() {
        let err: crate::DataError = TypeUrnError::Empty.into();
        assert_eq!(err.to_string(), TypeUrnError::Empty.to_string());
    }
}
