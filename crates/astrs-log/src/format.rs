//! Deterministic formatters for [`LogRecord`]: human text and JSON.
//!
//! Both formatters are pure functions of a [`LogRecord`]'s field values —
//! no wall-clock reads, no hashing-order-dependent iteration (`fields` is
//! a `BTreeMap`, so key order is always ascending) — so the same record
//! always renders to the same bytes. `astrs logs -f` and the format
//! golden tests both depend on that.

use astrs_time::HlcTimestamp;

use crate::error::{LogError, Result};
use crate::record::LogRecord;

/// Width the node-id column is left-padded to in [`human`] output. Names
/// longer than this are not truncated — only alignment is best-effort.
const NODE_COLUMN_WIDTH: usize = 12;

/// Renders an [`HlcTimestamp`]'s physical component as an RFC-3339-shaped
/// UTC timestamp (`YYYY-MM-DDTHH:MM:SS.NNNNNNNNNZ`) for [`human`]'s use.
///
/// [`HlcTimestamp`]'s own [`std::fmt::Display`] (`"<physical_ns>-<logical>"`)
/// is optimized for round-tripping through [`std::str::FromStr`] as a
/// compact sort/storage key, not for a person tailing `astrs logs -f` —
/// this function is the human-facing rendering this crate needs instead,
/// kept local to this module rather than added to `astrs-time` (which owns
/// the type but not this crate's log-line formatting conventions).
///
/// The logical tie-breaking counter is appended as `+<logical>` only when
/// it is nonzero, so the overwhelmingly common case (one record per
/// physical nanosecond) reads as a plain timestamp, while two records
/// sharing a physical instant stay visually distinguishable.
fn render_hlc(ts: HlcTimestamp) -> String {
    let physical = ts.physical_ns();
    let secs = physical / 1_000_000_000;
    let nanos = physical % 1_000_000_000;
    let days = (secs / 86_400) as i64;
    let day_secs = secs % 86_400;
    let (y, mo, d) = civil_from_days(days);
    let h = day_secs / 3600;
    let mi = (day_secs % 3600) / 60;
    let s = day_secs % 60;
    let mut out = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{nanos:09}Z");
    if ts.logical() != 0 {
        out.push('+');
        out.push_str(&ts.logical().to_string());
    }
    out
}

/// Days since the Unix epoch to a proleptic-Gregorian civil `(year, month,
/// day)` triple.
///
/// Howard Hinnant's `civil_from_days` algorithm — pure integer arithmetic,
/// correct for the entire range [`render_hlc`] needs (it is only ever fed
/// non-negative day counts derived from a `u64` nanosecond value), no
/// timezone database and no wall-clock read, which is what keeps
/// [`human`]'s output deterministic.
const fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    if m <= 2 { (y + 1, m, d) } else { (y, m, d) }
}

/// Renders `record` as one line of aligned human-readable text:
/// `<hlc> <LEVEL> <node> <target>: <message>`, followed by a compact
/// `{key=value, ...}` block if `fields` is non-empty.
///
/// # Examples
///
/// ```
/// use astrs_log::{HlcTimestamp, LogLevel, LogRecord, format};
///
/// let record = LogRecord::new(HlcTimestamp::new(0, 0), LogLevel::Warn, "astrs_daemon", "frame drop")
///     .with_node("camera")
///     .with_field("frame_id", 42);
///
/// assert_eq!(
///     format::human(&record),
///     "1970-01-01T00:00:00.000000000Z WARN  camera       astrs_daemon: frame drop  {frame_id=42}",
/// );
/// ```
#[must_use]
pub fn human(record: &LogRecord) -> String {
    let node = record.node.as_deref().unwrap_or("-");
    let mut out = format!(
        "{ts} {lvl} {node:<width$} {target}: {message}",
        ts = render_hlc(record.hlc),
        lvl = record.level.as_padded_str(),
        width = NODE_COLUMN_WIDTH,
        target = record.target,
        message = record.message,
    );
    if !record.fields.is_empty() {
        out.push_str("  {");
        for (i, (key, value)) in record.fields.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(key);
            out.push('=');
            out.push_str(&compact_value(value));
        }
        out.push('}');
    }
    out
}

/// Renders one field value compactly for [`human`]: bare (unquoted) for
/// simple strings, numbers, booleans and null; falls back to compact JSON
/// for strings needing escaping and for arrays/objects, so nested
/// structure is never silently lost.
fn compact_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) if is_bare_safe(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => "null".to_owned(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "<unserializable>".to_owned()),
    }
}

/// A string is safe to render bare (no surrounding quotes) in the compact
/// `key=value` field block if it is non-empty and contains only
/// characters that cannot be confused with the block's own `, ` and `=`
/// delimiters or JSON quoting.
fn is_bare_safe(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':'))
}

/// Renders `record` as one line of compact JSON — the exact
/// [`RotatingWriter`](crate::RotatingWriter) on-disk representation.
///
/// # Errors
///
/// Returns [`LogError::Encode`] if serialization fails (see that
/// variant's docs — effectively unreachable for well-formed records).
///
/// # Examples
///
/// ```
/// use astrs_log::{HlcTimestamp, LogLevel, LogRecord, format};
///
/// let record = LogRecord::new(HlcTimestamp::new(0, 0), LogLevel::Info, "t", "m");
/// let line = format::json(&record).unwrap();
/// assert!(line.starts_with('{') && line.ends_with('}'));
/// assert!(!line.contains('\n'));
/// ```
pub fn json(record: &LogRecord) -> Result<String> {
    serde_json::to_string(record).map_err(LogError::Encode)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::level::LogLevel;
    use astrs_time::HlcTimestamp;

    #[test]
    fn render_hlc_epoch_displays_as_1970() {
        assert_eq!(
            render_hlc(HlcTimestamp::new(0, 0)),
            "1970-01-01T00:00:00.000000000Z"
        );
    }

    #[test]
    fn render_hlc_known_instant_displays_correctly() {
        // 2024-01-01T00:00:00Z = 1704067200 seconds since epoch (verified
        // against the reference civil calendar).
        let ts = HlcTimestamp::new(1_704_067_200_000_000_000, 0);
        assert_eq!(render_hlc(ts), "2024-01-01T00:00:00.000000000Z");
    }

    #[test]
    fn render_hlc_known_instant_with_nanos_and_time_of_day() {
        // 2026-08-18T00:14:05.123456789Z (day count and rendering both
        // independently cross-checked against Python's `datetime`).
        let days: u64 = 20_683; // days from 1970-01-01 to 2026-08-18
        let secs = days * 86_400 + 14 * 60 + 5;
        assert_eq!(secs, 1_787_012_045);
        let ts = HlcTimestamp::new(secs * 1_000_000_000 + 123_456_789, 0);
        assert_eq!(render_hlc(ts), "2026-08-18T00:14:05.123456789Z");
    }

    #[test]
    fn render_hlc_appends_nonzero_logical_counter() {
        assert_eq!(
            render_hlc(HlcTimestamp::new(0, 5)),
            "1970-01-01T00:00:00.000000000Z+5"
        );
    }

    #[test]
    fn human_golden_no_node_no_fields() {
        let record = LogRecord::new(
            HlcTimestamp::new(0, 0),
            LogLevel::Info,
            "astrs_cli",
            "started",
        );
        assert_eq!(
            human(&record),
            "1970-01-01T00:00:00.000000000Z INFO  -            astrs_cli: started"
        );
    }

    #[test]
    fn human_golden_with_node_and_scalar_fields() {
        let record = LogRecord::new(
            HlcTimestamp::new(0, 0),
            LogLevel::Warn,
            "astrs_daemon",
            "frame drop",
        )
        .with_node("camera")
        .with_field("frame_id", 42)
        .with_field("dropped", true);
        assert_eq!(
            human(&record),
            "1970-01-01T00:00:00.000000000Z WARN  camera       astrs_daemon: frame drop  {dropped=true, frame_id=42}"
        );
    }

    #[test]
    fn human_golden_with_string_needing_quotes() {
        let record = LogRecord::new(HlcTimestamp::new(0, 0), LogLevel::Error, "t", "m")
            .with_field("reason", "timed out, retrying");
        assert_eq!(
            human(&record),
            "1970-01-01T00:00:00.000000000Z ERROR -            t: m  {reason=\"timed out, retrying\"}"
        );
    }

    #[test]
    fn human_golden_with_nested_field() {
        let record = LogRecord::new(HlcTimestamp::new(0, 0), LogLevel::Debug, "t", "m")
            .with_field("pose", serde_json::json!({"x": 1.0, "y": 2.0}));
        assert_eq!(
            human(&record),
            "1970-01-01T00:00:00.000000000Z DEBUG -            t: m  {pose={\"x\":1.0,\"y\":2.0}}"
        );
    }

    #[test]
    fn human_golden_with_nonzero_logical_counter() {
        let record = LogRecord::new(HlcTimestamp::new(0, 3), LogLevel::Info, "t", "m");
        assert_eq!(
            human(&record),
            "1970-01-01T00:00:00.000000000Z+3 INFO  -            t: m"
        );
    }

    #[test]
    fn human_output_is_independent_of_field_insertion_order() {
        let a = LogRecord::new(HlcTimestamp::new(0, 0), LogLevel::Info, "t", "m")
            .with_field("a", 1)
            .with_field("b", 2);
        let b = LogRecord::new(HlcTimestamp::new(0, 0), LogLevel::Info, "t", "m")
            .with_field("b", 2)
            .with_field("a", 1);
        assert_eq!(human(&a), human(&b));
    }

    #[test]
    fn node_column_does_not_truncate_long_names() {
        let record = LogRecord::new(HlcTimestamp::new(0, 0), LogLevel::Info, "t", "m")
            .with_node("a-very-long-node-identifier");
        assert!(human(&record).contains("a-very-long-node-identifier"));
    }

    #[test]
    fn json_golden_omits_defaults() {
        let record = LogRecord::new(HlcTimestamp::new(0, 0), LogLevel::Info, "t", "m");
        assert_eq!(
            json(&record).unwrap(),
            r#"{"hlc":{"physical_ns":0,"logical":0},"level":"info","target":"t","message":"m","seq":0}"#
        );
    }

    #[test]
    fn json_golden_with_node_seq_and_fields() {
        let record = LogRecord::new(HlcTimestamp::new(1, 2), LogLevel::Error, "t", "m")
            .with_node("n")
            .with_seq(9)
            .with_field("k", "v");
        assert_eq!(
            json(&record).unwrap(),
            r#"{"hlc":{"physical_ns":1,"logical":2},"level":"error","node":"n","target":"t","message":"m","seq":9,"fields":{"k":"v"}}"#
        );
    }

    #[test]
    fn json_output_never_contains_a_newline() {
        let record = LogRecord::new(
            HlcTimestamp::default(),
            LogLevel::Info,
            "t",
            "multi\nline message",
        )
        .with_field("note", "also\nmulti\nline");
        // serde_json escapes embedded newlines as \n within the string,
        // it never emits a literal line break -- which is exactly what
        // "line-oriented JSON" requires.
        assert!(!json(&record).unwrap().contains('\n'));
    }

    #[test]
    fn json_round_trips_through_format_and_back() {
        let record = LogRecord::new(HlcTimestamp::new(7, 0), LogLevel::Debug, "t", "m")
            .with_node("n")
            .with_field("k", 1);
        let line = json(&record).unwrap();
        let back: LogRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(record, back);
    }
}
