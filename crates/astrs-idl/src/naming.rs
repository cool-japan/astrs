//! ROS 2 identifier rules, type naming (`pkg/kind/Type`), and the mapping
//! from a ROS name to a Rust identifier or an `astrs-data` URN (blueprint
//! §10.3, §10.4's "name mangling" neighbour on the naming side).
//!
//! # Why generated types mint `std/ros2/v1/...` URNs
//!
//! [`astrs_data::AstrsMessage::URN`] documents its shape as
//! `std/<category>/v<n>/<Name>` (blueprint §24.3), and
//! [`astrs_data::TypeUrn::parse`] enforces exactly that: `TypeUrn::NAMESPACE`
//! is `"std"`, a closed set of one entry by design (its own doc comment
//! calls opening it to other namespaces "the designed extension point" —
//! *not yet taken*). §24.3's curated `std/{core,media,vision,geometry,
//! sensor,nav,time}/v1/*` registry is a small, hand-designed set that ROS
//! common_interfaces *map onto* via a bidirectional table blueprint §24.3
//! places in `astrs-ros2`; it does not have an entry for the ~90 ROS
//! message/service/action wire types `astrs-idl` mechanically generates a
//! type for.
//!
//! So every generated type mints its own URN in a **new category under the
//! existing namespace**, `std/ros2/v1/<Name>`, exactly the extension the
//! namespace's *category* segment already supports (nothing about adding a
//! category needs a `TypeUrn` change — [`astrs_data::urn::registry`] simply
//! has no entries for it, the same position the crate's own `Ping`/
//! `"std/core/v1/PingMsg"` test fixture is in). `<Name>` is the package name
//! upper-camel-cased and concatenated with the interface's own upper-camel
//! name — `geometry_msgs` + `Point` → `GeometryMsgsPoint` — which is
//! collision-free within one package (ROS type names are unique per
//! package) and across packages (distinct package prefixes). This is
//! `astrs-idl`'s own mechanical layer, independent of and a precursor to the
//! curated `std/*` semantic bridge table §24.3 assigns to `astrs-ros2`.

use std::fmt;

use astrs_data::TypeUrn;
use heck::ToUpperCamelCase;
use proc_macro2::{Ident, Span as MacroSpan};

use crate::error::IdlError;
use crate::span::Span;

/// Rust 2024's full keyword set — strict, reserved (`try`, `gen`), and the
/// three 2018-edition additions (`async`, `await`, `dyn`) — every one of
/// which becomes a legal identifier when raw-escaped with `r#`.
///
/// ROS 2 field/constant names are ASCII letters, digits and `_` only, so
/// none of Rust's *contextual* keywords (`union`, `macro_rules`, `raw`, …)
/// can collide in a way that needs handling here — those are only keywords
/// next to specific other tokens a bare field/const identifier never sits
/// beside.
const RUST_KEYWORDS: [&str; 52] = [
    "as", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
    "use", "where", "while", "async", "await", "gen", "try", "abstract", "become", "box", "do",
    "final", "macro", "override", "priv", "typeof", "unsized", "virtual", "yield",
];

/// The three identifiers that stay reserved even written raw (`r#self` is
/// not legal Rust) — ROS 2's naming rules keep field/constant names
/// lowercase-first or uppercase-first respectively, so only the
/// lowercase-legal two can ever actually arise from a parsed name; `Self`
/// is listed for completeness since [`to_rust_ident`] is not itself
/// case-restricted.
const UNESCAPABLE_EVEN_RAW: [&str; 3] = ["self", "super", "Self"];

/// Turns a ROS 2 field or constant name into a Rust identifier, handling the
/// two ways the two languages' identifier rules can disagree:
///
/// - A name that is also a Rust keyword (`type`, `move`, `true`, …) is
///   emitted as a raw identifier (`r#type`).
/// - The handful of names raw identifiers still cannot express (`self`,
///   `super`) get a trailing underscore instead, the same convention
///   `bindgen`/`prost` use for the same problem.
///
/// Every other name passes through unchanged. Message/service/action *type*
/// names never need this: ROS 2's `[A-Z][A-Za-z0-9]*` type-name grammar
/// cannot spell a Rust keyword (every one is lowercase).
#[must_use]
pub fn to_rust_ident(name: &str) -> Ident {
    if UNESCAPABLE_EVEN_RAW.contains(&name) {
        Ident::new(&format!("{name}_"), MacroSpan::call_site())
    } else if RUST_KEYWORDS.contains(&name) {
        Ident::new_raw(name, MacroSpan::call_site())
    } else {
        Ident::new(name, MacroSpan::call_site())
    }
}

/// The ROS 2 category folder a type lives in: `msg`, `srv` or `action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InterfaceKind {
    /// A `.msg` file, under `<package>/msg/`.
    Msg,
    /// A `.srv` file, under `<package>/srv/`.
    Srv,
    /// An `.action` file, under `<package>/action/`.
    Action,
}

impl InterfaceKind {
    /// The folder/middle-segment spelling: `"msg"`, `"srv"` or `"action"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Msg => "msg",
            Self::Srv => "srv",
            Self::Action => "action",
        }
    }

    /// The file extension this kind is parsed from, without the dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Msg => "msg",
            Self::Srv => "srv",
            Self::Action => "action",
        }
    }
}

impl fmt::Display for InterfaceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A validated ROS 2 package name: `^[a-z][a-z0-9_]*$`, no leading, trailing
/// or doubled underscore.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PackageName(String);

impl PackageName {
    /// Validates and wraps a package name.
    ///
    /// # Errors
    ///
    /// [`IdlError::InvalidPackageName`].
    pub fn new(name: impl Into<String>, span: Span) -> Result<Self, IdlError> {
        let name = name.into();
        if is_valid_lower_snake(&name) {
            Ok(Self(name))
        } else {
            Err(IdlError::InvalidPackageName { name, span })
        }
    }

    /// The package name text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The package name, upper-camel-cased (`geometry_msgs` →
    /// `GeometryMsgs`) — the prefix of every type this package mints a URN
    /// or Rust module for.
    #[must_use]
    pub fn upper_camel(&self) -> String {
        self.0.to_upper_camel_case()
    }
}

impl fmt::Display for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A fully qualified ROS 2 interface type name: `package/kind/Name`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeName {
    /// The owning package.
    pub package: PackageName,
    /// `msg`, `srv` or `action`.
    pub kind: InterfaceKind,
    /// The type's own name, upper-camel-case (`"Point"`, `"Trigger"`).
    pub name: String,
}

impl TypeName {
    /// Builds a type name, validating `name` as `^[A-Z][A-Za-z0-9]*$`.
    ///
    /// # Errors
    ///
    /// [`IdlError::InvalidTypeName`].
    pub fn new(
        package: PackageName,
        kind: InterfaceKind,
        name: impl Into<String>,
        span: Span,
    ) -> Result<Self, IdlError> {
        let name = name.into();
        if is_valid_upper_camel(&name) {
            Ok(Self {
                package,
                kind,
                name,
            })
        } else {
            Err(IdlError::InvalidTypeName { name, span })
        }
    }

    /// The canonical three-segment form: `"geometry_msgs/msg/Point"`.
    #[must_use]
    pub fn full(&self) -> String {
        format!("{}/{}/{}", self.package, self.kind.as_str(), self.name)
    }

    /// The two-segment form a field's type reference uses within the same
    /// interface family: `"geometry_msgs/Point"`.
    #[must_use]
    pub fn relative(&self) -> String {
        format!("{}/{}", self.package, self.name)
    }
}

impl fmt::Display for TypeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.full())
    }
}

/// `^[a-z][a-z0-9_]*$`, no leading/trailing/doubled underscore — ROS 2
/// package and field names (`rosidl_adapter.parser.validate_field_name`).
#[must_use]
pub fn is_valid_lower_snake(text: &str) -> bool {
    let Some(first) = text.chars().next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !text.ends_with('_')
        && !text.contains("__")
}

/// `^[A-Z][A-Z0-9_]*$`, no leading/trailing/doubled underscore — ROS 2
/// constant names.
#[must_use]
pub fn is_valid_upper_snake(text: &str) -> bool {
    let Some(first) = text.chars().next() else {
        return false;
    };
    first.is_ascii_uppercase()
        && text
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !text.ends_with('_')
        && !text.contains("__")
}

/// `^[A-Z][A-Za-z0-9]*$`, no underscores at all — ROS 2 message/service/
/// action type names (and, not coincidentally, `astrs_data::TypeUrn`'s own
/// `name` grammar).
#[must_use]
pub fn is_valid_upper_camel(text: &str) -> bool {
    let Some(first) = text.chars().next() else {
        return false;
    };
    first.is_ascii_uppercase() && text.chars().all(|c| c.is_ascii_alphanumeric())
}

/// True when `name` is one of the fifteen `.msg` scalar-type keywords
/// (`bool` … `wstring`) and therefore illegal as a field or constant name.
#[must_use]
pub fn is_reserved_type_keyword(name: &str) -> bool {
    const KEYWORDS: [&str; 15] = [
        "bool", "byte", "char", "float32", "float64", "int8", "uint8", "int16", "uint16", "int32",
        "uint32", "int64", "uint64", "string", "wstring",
    ];
    KEYWORDS.contains(&name)
}

/// Validates a field name.
///
/// # Errors
///
/// [`IdlError::InvalidFieldName`] when the shape is wrong,
/// [`IdlError::ReservedIdentifier`] when it names a primitive type.
pub fn validate_field_name(name: &str, span: Span) -> Result<(), IdlError> {
    if is_reserved_type_keyword(name) {
        return Err(IdlError::ReservedIdentifier {
            name: name.to_owned(),
            kind: "primitive type",
            span,
        });
    }
    if is_valid_lower_snake(name) {
        Ok(())
    } else {
        Err(IdlError::InvalidFieldName {
            name: name.to_owned(),
            span,
        })
    }
}

/// Validates a constant name.
///
/// # Errors
///
/// [`IdlError::InvalidConstantName`].
pub fn validate_constant_name(name: &str, span: Span) -> Result<(), IdlError> {
    if is_valid_upper_snake(name) {
        Ok(())
    } else {
        Err(IdlError::InvalidConstantName {
            name: name.to_owned(),
            span,
        })
    }
}

/// Mints this generated type's `astrs_data::AstrsMessage::URN` — see the
/// [module documentation](self) for why the category is `"ros2"`.
///
/// # Errors
///
/// [`IdlError::UrnNameTooLong`] when `package` upper-camel-cased and
/// concatenated with `type_name` would exceed
/// [`TypeUrn::MAX_SEGMENT_LEN`].
pub fn mint_urn(package: &PackageName, type_name: &str, span: Span) -> Result<TypeUrn, IdlError> {
    let name = format!("{}{type_name}", package.upper_camel());
    TypeUrn::try_new("ros2", 1, &name).map_err(|_source| IdlError::UrnNameTooLong {
        len: name.len(),
        urn: name,
        max: TypeUrn::MAX_SEGMENT_LEN,
        span,
    })
}

/// The ROS 2 / DDS-XTypes mangled type name a SEDP
/// `PID_TYPE_NAME`/`type_name` announcement uses:
/// `"<package>::<kind>::dds_::<Name>_"` (`rosidl`'s own convention — see
/// e.g. `std_msgs::msg::dds_::String_`). Consumed by `astrs-rtps`/
/// `astrs-ros2` once they exist; generated here because minting it needs
/// exactly the package/kind/name triple `astrs-idl` already has in hand.
#[must_use]
pub fn dds_type_name(type_name: &TypeName) -> String {
    format!(
        "{}::{}::dds_::{}_",
        type_name.package,
        type_name.kind.as_str(),
        type_name.name
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::span::Position;

    fn at() -> Span {
        Span::empty(Position::new(1, 1))
    }

    #[test]
    fn lower_snake_accepts_ros_package_and_field_names() {
        assert!(is_valid_lower_snake("geometry_msgs"));
        assert!(is_valid_lower_snake("x"));
        assert!(is_valid_lower_snake("point_32"));
        assert!(!is_valid_lower_snake(""));
        assert!(!is_valid_lower_snake("Geometry"));
        assert!(!is_valid_lower_snake("_leading"));
        assert!(!is_valid_lower_snake("trailing_"));
        assert!(!is_valid_lower_snake("double__underscore"));
        assert!(!is_valid_lower_snake("9start"));
    }

    #[test]
    fn upper_snake_accepts_ros_constant_names() {
        assert!(is_valid_upper_snake("STATUS_OK"));
        assert!(is_valid_upper_snake("X"));
        assert!(!is_valid_upper_snake("status_ok"));
        assert!(!is_valid_upper_snake("_X"));
        assert!(!is_valid_upper_snake("X_"));
        assert!(!is_valid_upper_snake("X__Y"));
    }

    #[test]
    fn upper_camel_accepts_ros_type_names() {
        assert!(is_valid_upper_camel("Point"));
        assert!(is_valid_upper_camel("PointCloud2"));
        assert!(!is_valid_upper_camel("point"));
        assert!(!is_valid_upper_camel("Point_Cloud"));
        assert!(!is_valid_upper_camel(""));
    }

    #[test]
    fn reserved_type_keywords_are_recognised() {
        assert!(is_reserved_type_keyword("int32"));
        assert!(is_reserved_type_keyword("wstring"));
        assert!(!is_reserved_type_keyword("int33"));
        assert!(!is_reserved_type_keyword("velocity"));
    }

    #[test]
    fn validate_field_name_rejects_a_type_keyword() {
        assert_eq!(
            validate_field_name("int32", at()),
            Err(IdlError::ReservedIdentifier {
                name: "int32".to_owned(),
                kind: "primitive type",
                span: at(),
            })
        );
    }

    #[test]
    fn validate_field_name_accepts_header_lowercase() {
        assert_eq!(validate_field_name("header", at()), Ok(()));
    }

    #[test]
    fn package_name_upper_camels_underscored_segments() {
        let pkg = PackageName::new("geometry_msgs", at()).unwrap();
        assert_eq!(pkg.upper_camel(), "GeometryMsgs");
        let pkg = PackageName::new("builtin_interfaces", at()).unwrap();
        assert_eq!(pkg.upper_camel(), "BuiltinInterfaces");
        let pkg = PackageName::new("std_msgs", at()).unwrap();
        assert_eq!(pkg.upper_camel(), "StdMsgs");
    }

    #[test]
    fn type_name_full_and_relative_forms() {
        let package = PackageName::new("geometry_msgs", at()).unwrap();
        let type_name = TypeName::new(package, InterfaceKind::Msg, "Point", at()).unwrap();
        assert_eq!(type_name.full(), "geometry_msgs/msg/Point");
        assert_eq!(type_name.relative(), "geometry_msgs/Point");
        assert_eq!(type_name.to_string(), "geometry_msgs/msg/Point");
    }

    #[test]
    fn mint_urn_produces_a_parseable_std_ros2_urn() {
        let package = PackageName::new("geometry_msgs", at()).unwrap();
        let urn = mint_urn(&package, "Point", at()).unwrap();
        assert_eq!(urn.as_str(), "std/ros2/v1/GeometryMsgsPoint");
        assert_eq!(urn.category(), "ros2");
        assert_eq!(urn.version(), 1);
        assert_eq!(urn.name(), "GeometryMsgsPoint");
    }

    #[test]
    fn mint_urn_rejects_a_name_over_the_segment_limit() {
        let package = PackageName::new("a", at()).unwrap();
        let huge_name = "X".repeat(TypeUrn::MAX_SEGMENT_LEN);
        let err = mint_urn(&package, &huge_name, at()).unwrap_err();
        assert!(matches!(err, IdlError::UrnNameTooLong { .. }));
    }

    #[test]
    fn dds_type_name_matches_the_rosidl_mangled_form() {
        let package = PackageName::new("std_msgs", at()).unwrap();
        let type_name = TypeName::new(package, InterfaceKind::Msg, "String", at()).unwrap();
        assert_eq!(dds_type_name(&type_name), "std_msgs::msg::dds_::String_");
    }

    #[test]
    fn ordinary_names_pass_through_unchanged() {
        assert_eq!(to_rust_ident("velocity").to_string(), "velocity");
        assert_eq!(to_rust_ident("STATUS_OK").to_string(), "STATUS_OK");
    }

    #[test]
    fn rust_keywords_are_raw_escaped() {
        assert_eq!(to_rust_ident("type").to_string(), "r#type");
        assert_eq!(to_rust_ident("move").to_string(), "r#move");
        assert_eq!(to_rust_ident("true").to_string(), "r#true");
        assert_eq!(to_rust_ident("loop").to_string(), "r#loop");
    }

    #[test]
    fn unescapable_keywords_get_a_trailing_underscore_instead() {
        assert_eq!(to_rust_ident("self").to_string(), "self_");
        assert_eq!(to_rust_ident("super").to_string(), "super_");
    }

    #[test]
    fn dds_type_name_covers_srv_and_action_kinds() {
        let package = PackageName::new("example_interfaces", at()).unwrap();
        let srv = TypeName::new(package, InterfaceKind::Srv, "AddTwoInts", at()).unwrap();
        assert_eq!(
            dds_type_name(&srv),
            "example_interfaces::srv::dds_::AddTwoInts_"
        );
    }

    // ---- Property tests -----------------------------------------------

    use proptest::prelude::*;

    proptest! {
        /// [`to_rust_ident`]'s whole contract is "the output is always a
        /// legal Rust identifier" — checked here by feeding it back through
        /// `syn`'s own identifier parser across every ROS 2 field/constant
        /// name shape (lower_snake and UPPER_SNAKE), not just the dozen
        /// keyword collisions the unit tests above name individually.
        #[test]
        fn to_rust_ident_always_produces_a_legal_rust_identifier(
            name in prop_oneof![
                "[a-z][a-z0-9]{0,8}(_[a-z0-9]{1,8}){0,3}",
                "[A-Z][A-Z0-9]{0,8}(_[A-Z0-9]{1,8}){0,3}",
            ],
        ) {
            let rendered = to_rust_ident(&name).to_string();
            prop_assert!(
                syn::parse_str::<syn::Ident>(&rendered).is_ok(),
                "{name:?} -> {rendered:?} is not a legal Rust identifier"
            );
        }

        /// [`mint_urn`] round-trips through [`TypeUrn::parse`] for any
        /// package/type-name pair whose minted name stays within the URN
        /// segment limit — the success-path counterpart to
        /// `mint_urn_rejects_a_name_over_the_segment_limit` above, which
        /// covers the one boundary this property deliberately stays clear
        /// of (kept as its own hand-written test since the failure shape,
        /// not a round trip, is what that boundary is about).
        #[test]
        fn mint_urn_round_trips_through_type_urn_parse(
            package_name in "[a-z][a-z0-9]{0,4}(_[a-z0-9]{1,4}){0,3}",
            type_name in "[A-Z][A-Za-z0-9]{0,15}",
        ) {
            let package = PackageName::new(package_name, at())
                .expect("generator only produces valid lower_snake package names");
            let minted_len = package.upper_camel().len() + type_name.len();
            prop_assume!(minted_len <= TypeUrn::MAX_SEGMENT_LEN);

            let urn = mint_urn(&package, &type_name, at()).expect("within the segment limit");
            let parsed = TypeUrn::parse(urn.as_str()).expect("mint_urn always produces a parseable URN");
            prop_assert_eq!(parsed.as_str(), urn.as_str());
            prop_assert_eq!(parsed.category(), "ros2");
            prop_assert_eq!(parsed.version(), 1);
        }

        /// `is_valid_lower_snake` never agrees with `is_valid_upper_snake`
        /// or `is_valid_upper_camel` on the same text: all three require a
        /// specific case for the first character, lowercase for the first
        /// and uppercase for the other two, so a shared match would need a
        /// first character that is simultaneously ASCII-lowercase and
        /// ASCII-uppercase. (`is_valid_upper_snake` and
        /// `is_valid_upper_camel` are a *different* story — a name that is
        /// only uppercase letters and digits, `"A"` or `"OK"`, legitimately
        /// satisfies both; the field/constant grammars never collide in
        /// practice all the same, since `crate::parser` dispatches on the
        /// first character's case alone, never on which of these two a name
        /// happens to also match.)
        #[test]
        fn lower_snake_never_overlaps_the_upper_case_shapes(text in ".{0,24}") {
            prop_assert!(!(is_valid_lower_snake(&text) && is_valid_upper_snake(&text)));
            prop_assert!(!(is_valid_lower_snake(&text) && is_valid_upper_camel(&text)));
        }
    }
}
