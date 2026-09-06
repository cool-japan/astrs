//! Node/graph environment values and `$VAR` expansion (blueprint §8.3).
//!
//! `env:` maps in the manifest accept scalars typed as string, bool, int or
//! float — `CAMERA_INDEX: 0` should round-trip as the integer `0`, not the
//! string `"0"`. [`EnvValue`] models that; [`expand_str`] and [`expand_map`]
//! implement `$VAR` / `${VAR}` substitution against a caller-supplied lookup
//! function, **never** the process environment directly (the daemon decides
//! what a node is allowed to see; this library must not leak the host
//! environment into that decision).

use std::collections::BTreeMap;
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A single environment value as written in a manifest `env:` map.
///
/// Serialized untagged: a YAML scalar decides its own variant by its
/// resolved YAML tag (`CAMERA_INDEX: 0` is `Int(0)`; `CAMERA_INDEX: "0"` is
/// `String("0")`), matching ordinary YAML type resolution rather than
/// re-inferring types from string content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum EnvValue {
    /// A string value; the only variant eligible for `$VAR` expansion.
    String(String),
    /// A boolean value (`true` / `false`).
    Bool(bool),
    /// A signed 64-bit integer value.
    Int(i64),
    /// A 64-bit floating point value.
    Float(f64),
}

impl EnvValue {
    /// Render this value as the literal string a spawned process would
    /// receive, with **no** `$VAR` expansion applied.
    ///
    /// Booleans render as `true`/`false`; integers and floats use their
    /// ordinary `Display` formatting.
    #[must_use]
    pub fn to_raw_string(&self) -> String {
        match self {
            Self::String(s) => s.clone(),
            Self::Bool(b) => b.to_string(),
            Self::Int(i) => i.to_string(),
            Self::Float(f) => f.to_string(),
        }
    }

    /// Whether this value is eligible for `$VAR` expansion.
    ///
    /// Only [`EnvValue::String`] ever contains variable references; the
    /// other variants are returned unexpanded by [`EnvValue::expand`].
    #[must_use]
    pub fn is_expandable(&self) -> bool {
        matches!(self, Self::String(_))
    }

    /// Expand `$VAR` / `${VAR}` references in this value using `lookup`,
    /// escaping `$$` to a literal `$`.
    ///
    /// Non-string variants pass through as their raw string form
    /// unchanged — a bare integer or boolean has nothing to expand.
    ///
    /// # Errors
    ///
    /// Returns [`EnvExpandError`] if a referenced variable is undefined, or
    /// if a `$`-expression is malformed (an unterminated `${`, or a bare
    /// `$` not followed by `$`, `{`, or an identifier character).
    pub fn expand(
        &self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<String, EnvExpandError> {
        match self {
            Self::String(s) => expand_str(s, lookup),
            other => Ok(other.to_raw_string()),
        }
    }
}

impl fmt::Display for EnvValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_raw_string())
    }
}

/// An error raised while expanding `$VAR` references in an [`EnvValue`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EnvExpandError {
    /// `$NAME` or `${NAME}` referenced a variable the lookup function does
    /// not know about.
    #[error("undefined variable `{0}` referenced in environment value")]
    UndefinedVariable(String),

    /// A `${` was never closed with a matching `}`.
    #[error("unterminated `${{` in environment value (missing closing `}}`)")]
    UnterminatedBrace,

    /// A bare `$` was not followed by `$`, `{`, or an identifier start
    /// character (`[A-Za-z_]`).
    #[error("`$` at byte offset {0} is not followed by a variable name, `{{`, or `$`")]
    DanglingDollar(usize),

    /// `${}` or `$` with an empty brace body — no variable name given.
    #[error("empty variable name in `${{}}`")]
    EmptyVariableName,
}

/// Expand `$VAR` / `${VAR}` references in `input` using `lookup`.
///
/// See [`EnvValue::expand`] for the escaping and error semantics; this
/// free function is the same engine, usable directly on a plain string
/// (for example, `working_dir` or `build` lines that also document
/// variable expansion informally, without needing to route through an
/// [`EnvValue`]).
///
/// # Errors
///
/// See [`EnvValue::expand`].
pub fn expand_str(
    input: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<String, EnvExpandError> {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        if b != b'$' {
            // Advance by one UTF-8 scalar, not one byte, so multi-byte
            // characters are copied whole.
            let ch_len = utf8_char_len(input, i);
            out.push_str(&input[i..i + ch_len]);
            i += ch_len;
            continue;
        }

        // b == '$'
        let next = bytes.get(i + 1).copied();
        match next {
            Some(b'$') => {
                out.push('$');
                i += 2;
            }
            Some(b'{') => {
                let close = input[i + 2..]
                    .find('}')
                    .ok_or(EnvExpandError::UnterminatedBrace)?;
                let name = &input[i + 2..i + 2 + close];
                if name.is_empty() {
                    return Err(EnvExpandError::EmptyVariableName);
                }
                let value = lookup(name)
                    .ok_or_else(|| EnvExpandError::UndefinedVariable(name.to_string()))?;
                out.push_str(&value);
                i += 2 + close + 1;
            }
            Some(c) if is_ident_start(c) => {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() && is_ident_continue(bytes[end]) {
                    end += 1;
                }
                let name = &input[start..end];
                let value = lookup(name)
                    .ok_or_else(|| EnvExpandError::UndefinedVariable(name.to_string()))?;
                out.push_str(&value);
                i = end;
            }
            _ => return Err(EnvExpandError::DanglingDollar(i)),
        }
    }

    Ok(out)
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Length in bytes of the UTF-8 scalar starting at byte offset `i` in `s`.
fn utf8_char_len(s: &str, i: usize) -> usize {
    match s[i..].chars().next() {
        Some(c) => c.len_utf8(),
        None => 1,
    }
}

/// Expand every value in an `env:` map, dropping non-expandable values
/// through unchanged.
///
/// The result maps each key to the fully expanded string that a spawned
/// process would receive. Expansion stops at the first error; combined
/// with [`crate::ValidationErrors`]-style multi-error reporting is
/// intentionally out of scope here, since expansion is a runtime spawn-time
/// concern (owned by `astrs-daemon`), not a manifest-validation concern —
/// this helper exists so that concern has a single, tested implementation
/// to call into rather than reinventing `$VAR` parsing per crate.
///
/// # Errors
///
/// Returns the first [`EnvExpandError`] encountered, tagged with the map
/// key it occurred on via [`EnvMapExpandError`].
pub fn expand_map(
    env: &BTreeMap<String, EnvValue>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<BTreeMap<String, String>, EnvMapExpandError> {
    let mut out = BTreeMap::new();
    for (key, value) in env {
        let expanded = value.expand(&lookup).map_err(|source| EnvMapExpandError {
            key: key.clone(),
            source,
        })?;
        out.insert(key.clone(), expanded);
    }
    Ok(out)
}

/// An [`EnvExpandError`] tagged with the `env:` map key it occurred on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("environment variable `{key}`: {source}")]
pub struct EnvMapExpandError {
    /// The `env:` map key whose value failed to expand.
    pub key: String,
    /// The underlying expansion error.
    #[source]
    pub source: EnvExpandError,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn lookup(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn bare_var_expands() {
        let out = expand_str("hello $NAME!", lookup(&[("NAME", "world")])).unwrap();
        assert_eq!(out, "hello world!");
    }

    #[test]
    fn braced_var_expands_with_trailing_text() {
        let out = expand_str("${GREETING}, world", lookup(&[("GREETING", "hi")])).unwrap();
        assert_eq!(out, "hi, world");
    }

    #[test]
    fn braced_var_allows_adjacent_text_no_word_boundary() {
        let out = expand_str("pre${X}post", lookup(&[("X", "MID")])).unwrap();
        assert_eq!(out, "preMIDpost");
    }

    #[test]
    fn dollar_dollar_escapes_to_literal_dollar() {
        let out = expand_str("price: $$5", lookup(&[])).unwrap();
        assert_eq!(out, "price: $5");
    }

    #[test]
    fn undefined_variable_errors() {
        let err = expand_str("$MISSING", lookup(&[])).unwrap_err();
        assert_eq!(
            err,
            EnvExpandError::UndefinedVariable("MISSING".to_string())
        );
    }

    #[test]
    fn unterminated_brace_errors() {
        let err = expand_str("${OOPS", lookup(&[])).unwrap_err();
        assert_eq!(err, EnvExpandError::UnterminatedBrace);
    }

    #[test]
    fn empty_braces_error() {
        let err = expand_str("${}", lookup(&[])).unwrap_err();
        assert_eq!(err, EnvExpandError::EmptyVariableName);
    }

    #[test]
    fn dangling_dollar_errors() {
        let err = expand_str("cost: $5", lookup(&[])).unwrap_err();
        assert_eq!(err, EnvExpandError::DanglingDollar(6));
    }

    #[test]
    fn trailing_dollar_errors() {
        let err = expand_str("trailing$", lookup(&[])).unwrap_err();
        assert_eq!(err, EnvExpandError::DanglingDollar(8));
    }

    #[test]
    fn non_string_values_pass_through_unexpanded() {
        assert_eq!(EnvValue::Int(0).expand(lookup(&[])).unwrap(), "0");
        assert_eq!(EnvValue::Bool(true).expand(lookup(&[])).unwrap(), "true");
        assert_eq!(EnvValue::Float(1.5).expand(lookup(&[])).unwrap(), "1.5");
    }

    #[test]
    fn multibyte_text_survives_expansion() {
        let out = expand_str("こんにちは $NAME さん", lookup(&[("NAME", "太郎")])).unwrap();
        assert_eq!(out, "こんにちは 太郎 さん");
    }

    #[test]
    fn expand_map_collects_all_keys() {
        let mut env = BTreeMap::new();
        env.insert("A".to_string(), EnvValue::String("$X".to_string()));
        env.insert("B".to_string(), EnvValue::Int(42));
        let out = expand_map(&env, lookup(&[("X", "expanded")])).unwrap();
        assert_eq!(out.get("A").map(String::as_str), Some("expanded"));
        assert_eq!(out.get("B").map(String::as_str), Some("42"));
    }

    #[test]
    fn expand_map_reports_offending_key() {
        let mut env = BTreeMap::new();
        env.insert("BAD".to_string(), EnvValue::String("$NOPE".to_string()));
        let err = expand_map(&env, lookup(&[])).unwrap_err();
        assert_eq!(err.key, "BAD");
    }

    #[test]
    fn untagged_deserialize_preserves_yaml_scalar_type() {
        let v: EnvValue = astrs_yaml::from_str("0").unwrap();
        assert_eq!(v, EnvValue::Int(0));
        let v: EnvValue = astrs_yaml::from_str("\"0\"").unwrap();
        assert_eq!(v, EnvValue::String("0".to_string()));
        let v: EnvValue = astrs_yaml::from_str("true").unwrap();
        assert_eq!(v, EnvValue::Bool(true));
        let v: EnvValue = astrs_yaml::from_str("1.5").unwrap();
        assert_eq!(v, EnvValue::Float(1.5));
    }
}
