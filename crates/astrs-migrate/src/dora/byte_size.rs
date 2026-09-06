//! Byte-size parsing matching dora's own `ByteSize::from_str` grammar
//! exactly (confirmed against `dora_message::config::ByteSize` in the
//! reference checkout, not guessed): a bare integer (raw bytes), or a
//! number followed by a `B`/`KB`/`MB`/`GB` unit suffix — case-insensitive,
//! and **binary** multipliers (`KB` is 1024, not 1000), despite the
//! decimal-looking name.

use super::model::DoraByteSize;

/// Why [`parse_byte_size`] rejected an input string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum ByteSizeError {
    /// The unit suffix was not one of `B`/`KB`/`MB`/`GB`.
    #[error("unknown byte size unit `{0}`")]
    UnknownUnit(String),
    /// The numeric part was not a valid number.
    #[error("invalid byte size `{0}`")]
    InvalidNumber(String),
    /// The numeric part was negative or non-finite.
    #[error("byte size must be a non-negative, finite number: `{0}`")]
    NotFiniteOrNegative(String),
    /// The value overflows `u64` once the unit multiplier is applied.
    #[error("byte size `{0}` is too large")]
    Overflow(String),
}

/// Parse a byte size the way dora's `ByteSize` does: `s.trim()`, split at
/// the first alphabetic character into a numeric part and a unit part; no
/// unit means "already in bytes". An integer numeric part is parsed and
/// multiplied exactly (`checked_mul`, never floating point) to avoid
/// rounding above 2^53; a fractional numeric part (`"1.5 KB"`) falls back
/// to `f64` multiplication.
///
/// # Errors
///
/// See [`ByteSizeError`].
pub(crate) fn parse_byte_size(raw: &str) -> Result<u64, ByteSizeError> {
    let s = raw.trim();
    let Some(split_at) = s.find(|c: char| c.is_alphabetic()) else {
        return s
            .parse::<u64>()
            .map_err(|_| ByteSizeError::InvalidNumber(s.to_string()));
    };
    let (num_part, unit_part) = (s[..split_at].trim(), s[split_at..].trim());

    let multiplier: u64 = match unit_part.to_uppercase().as_str() {
        "B" => 1,
        "KB" | "K" => 1024,
        "MB" | "M" => 1024 * 1024,
        "GB" | "G" => 1024 * 1024 * 1024,
        other => return Err(ByteSizeError::UnknownUnit(other.to_string())),
    };

    if let Ok(num) = num_part.parse::<u64>() {
        return num
            .checked_mul(multiplier)
            .ok_or_else(|| ByteSizeError::Overflow(s.to_string()));
    }

    let num: f64 = num_part
        .parse()
        .map_err(|_| ByteSizeError::InvalidNumber(num_part.to_string()))?;
    if !num.is_finite() || num < 0.0 {
        return Err(ByteSizeError::NotFiniteOrNegative(s.to_string()));
    }
    let bytes = num * multiplier as f64;
    if bytes >= u64::MAX as f64 {
        return Err(ByteSizeError::Overflow(s.to_string()));
    }
    // `bytes` was just checked to be finite, non-negative and below
    // `u64::MAX`, so this cast is exact-enough (sub-byte fractions round
    // down) and never saturates or produces `NaN`-driven `0` unexpectedly.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(bytes as u64)
}

/// Resolve a [`DoraByteSize`] (already-typed integer, or a string dora
/// would parse at its own YAML-deserialize boundary) to a byte count.
///
/// # Errors
///
/// See [`ByteSizeError`].
pub(crate) fn resolve_byte_size(value: &DoraByteSize) -> Result<u64, ByteSizeError> {
    match value {
        DoraByteSize::Int(bytes) => Ok(*bytes),
        DoraByteSize::Text(text) => parse_byte_size(text),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn bare_integer_is_bytes() {
        assert_eq!(parse_byte_size("1024").unwrap(), 1024);
    }

    #[test]
    fn kb_mb_gb_use_binary_multipliers() {
        assert_eq!(parse_byte_size("1KB").unwrap(), 1024);
        assert_eq!(parse_byte_size("64MB").unwrap(), 64 * 1024 * 1024);
        assert_eq!(parse_byte_size("1GB").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn single_letter_units_are_accepted() {
        assert_eq!(parse_byte_size("1K").unwrap(), 1024);
        assert_eq!(parse_byte_size("1M").unwrap(), 1024 * 1024);
    }

    #[test]
    fn unit_is_case_insensitive() {
        assert_eq!(parse_byte_size("1kb").unwrap(), 1024);
        assert_eq!(parse_byte_size("1Kb").unwrap(), 1024);
    }

    #[test]
    fn fractional_values_use_float_multiplication() {
        assert_eq!(parse_byte_size("1.5 KB").unwrap(), 1536);
    }

    #[test]
    fn whitespace_between_number_and_unit_is_tolerated() {
        assert_eq!(parse_byte_size("128 MB").unwrap(), 128 * 1024 * 1024);
    }

    #[test]
    fn unknown_unit_is_rejected() {
        assert_eq!(
            parse_byte_size("1TB").unwrap_err(),
            ByteSizeError::UnknownUnit("TB".to_string())
        );
    }

    #[test]
    fn negative_numbers_are_rejected() {
        assert!(matches!(
            parse_byte_size("-1KB"),
            Err(ByteSizeError::NotFiniteOrNegative(_))
        ));
    }

    #[test]
    fn garbage_unit_is_rejected_as_unknown_not_a_bad_number() {
        // The split happens at the *first* alphabetic character, exactly
        // like dora's own `ByteSize::from_str` -- so `"abcKB"` puts all of
        // `abcKB` into the unit part (empty numeric part), not `"abc"`
        // into the number and `"KB"` into the unit.
        assert_eq!(
            parse_byte_size("abcKB").unwrap_err(),
            ByteSizeError::UnknownUnit("ABCKB".to_string())
        );
    }

    #[test]
    fn garbage_number_before_a_valid_unit_is_rejected() {
        assert!(matches!(
            parse_byte_size("1.2.3KB"),
            Err(ByteSizeError::InvalidNumber(_))
        ));
    }

    #[test]
    fn resolve_passes_through_typed_integer() {
        assert_eq!(resolve_byte_size(&DoraByteSize::Int(42)).unwrap(), 42);
    }

    #[test]
    fn resolve_parses_typed_string() {
        assert_eq!(
            resolve_byte_size(&DoraByteSize::Text("1KB".to_string())).unwrap(),
            1024
        );
    }
}
