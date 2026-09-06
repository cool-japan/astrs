//! `.arec ⇄ bag` conversion (blueprint §10.6, §14): `astrs bag convert`.
//!
//! [`convert_bag`] is the single entry point `astrs bag convert <in> <out>`
//! drives, dispatching on both paths' extensions:
//!
//! | Input | Output | Direction |
//! |---|---|---|
//! | `.arec` | `.db3` | [`arec_to_db3::arec_to_db3`] |
//! | `.db3` | `.arec` | [`db3_to_arec::db3_to_arec`] |
//! | `.mcap` | `.arec` | [`mcap_to_arec::mcap_to_arec`] |
//! | anything | `.mcap` | refused — [`RosbagError::McapWriteUnsupported`] (no `.mcap` writer) |
//! | anything else | | refused — [`RosbagError::UnsupportedConversion`] |
//!
//! # Round-trip identity
//!
//! `.arec → .db3 → .arec'` preserves more than message content: the
//! source `.arec`'s [`astrs_wire::DataflowId`], its
//! [`astrs_time::HlcTimestamp`] header epoch, and every topic's exact
//! astrs node/output ids all survive, via [`crate::RosbagTopicManifest`]
//! embedded twice — once directly in the destination `.arec`'s
//! [`astrs_recording::Header::manifest_yaml`] (recovered by a later
//! `.arec → .db3` conversion of *that* file), and once inside a `.db3`'s
//! [`astrs_rosbag::db3::BagMetadata::custom_data`](crate::db3::BagMetadata::custom_data)
//! (which [`crate::db3::Writer::finish`] duplicates into both
//! `metadata.yaml` and the in-database `metadata` table, so it survives a
//! bag copied without its sidecar file too — see
//! [`crate::db3::Reader::metadata`]'s fallback).
//!
//! A `.db3` that was never produced by this crate's own
//! [`arec_to_db3::arec_to_db3`] carries no such `custom_data` entry, so
//! [`db3_to_arec::db3_to_arec`] falls back to [`crate::assign_node_ids`]
//! and a fresh [`astrs_wire::DataflowId::generate`] — the same fallback
//! [`mcap_to_arec::mcap_to_arec`] always uses (`.mcap` never had a
//! sidecar mechanism to write one into in the first place, and there is
//! no `.arec → .mcap` direction for one to round-trip through).
//!
//! # What does not round-trip
//!
//! Stated plainly rather than silently dropped:
//!
//! - **The HLC logical counter.** Only `physical_ns` survives into a
//!   `.db3` row's signed `timestamp` or an `.mcap` record's `log_time`;
//!   two `.arec` entries sharing a physical nanosecond but different
//!   logical counters land on the identical bag timestamp. Message
//!   *order* is still preserved (both formats' readers see rows/records
//!   in the order this crate wrote them — [`astrs_recording::Reader`]'s
//!   own HLC order for the source, on-disk order for the destination),
//!   only the sub-nanosecond tie-break value itself is lost.
//! - **Timestamps outside the target's range.** A `.db3` `timestamp` is a
//!   signed 64 bits; an `.arec` `physical_ns` over [`i64::MAX`] saturates
//!   to it (year 2262-ish and beyond — [`astrs_time::HlcTimestamp`] itself
//!   ranges to the year ~2554). A `.db3` pre-epoch (negative) `timestamp`
//!   saturates to physical nanosecond zero going the other way. Both are
//!   counted and reported in [`ConversionReport::warnings`], never
//!   silently wrapped.
//! - **Topics this crate cannot confirm are CDR.** A synthesized topic's
//!   `serialization_format` is inferred by peeking its first message's
//!   leading four bytes for a CDR encapsulation header
//!   ([`astrs_cdr::EncapsulationHeader::from_bytes`]) — never a full
//!   decode, and never fabricated as `"cdr"` on a payload this crate did
//!   not confirm (see [`crate::RAW_SERIALIZATION_FORMAT`]'s own docs).

pub mod arec_to_db3;
pub mod db3_to_arec;
pub mod mcap_to_arec;

use std::path::{Path, PathBuf};

pub use arec_to_db3::arec_to_db3;
pub use db3_to_arec::db3_to_arec;
pub use mcap_to_arec::mcap_to_arec;

use crate::error::RosbagError;

/// The three file shapes this crate reads or writes, sniffed from a
/// path's extension by [`detect_format`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BagFormat {
    /// `.arec` — [`astrs_recording`]'s own container (blueprint §14).
    Arec,
    /// `.db3` — rosbag2's SQLite storage plugin (read and write).
    Db3,
    /// `.mcap` — read-only in this crate (blueprint §10.6).
    Mcap,
}

impl BagFormat {
    /// This format's canonical extension, without the leading dot.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_rosbag::convert::BagFormat;
    ///
    /// assert_eq!(BagFormat::Db3.extension(), "db3");
    /// ```
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Arec => "arec",
            Self::Db3 => "db3",
            Self::Mcap => "mcap",
        }
    }
}

/// Sniffs `path`'s [`BagFormat`] from its extension, case-insensitively.
///
/// # Errors
///
/// [`RosbagError::UnknownExtension`] if `path` has no extension at all, or
/// [`RosbagError::UnsupportedExtension`] if it has one this crate does not
/// read or write.
///
/// # Examples
///
/// ```
/// use astrs_rosbag::convert::{BagFormat, detect_format};
/// use std::path::Path;
///
/// assert_eq!(detect_format(Path::new("session.ARec")).unwrap(), BagFormat::Arec);
/// assert!(detect_format(Path::new("session.txt")).is_err());
/// ```
pub fn detect_format(path: &Path) -> Result<BagFormat, RosbagError> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .ok_or_else(|| RosbagError::UnknownExtension {
            path: path.to_path_buf(),
        })?
        .to_ascii_lowercase();
    match extension.as_str() {
        "arec" => Ok(BagFormat::Arec),
        "db3" => Ok(BagFormat::Db3),
        "mcap" => Ok(BagFormat::Mcap),
        _ => Err(RosbagError::UnsupportedExtension {
            path: path.to_path_buf(),
            extension,
        }),
    }
}

/// What one [`convert_bag`] call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionReport {
    /// The file that was read.
    pub input: PathBuf,
    /// The file that was written.
    pub output: PathBuf,
    /// A short, human-readable label for the direction taken, e.g.
    /// `"arec -> db3"`.
    pub direction: &'static str,
    /// How many distinct topics were written.
    pub topics: usize,
    /// How many messages were written.
    pub messages: u64,
    /// Non-fatal notes: topics synthesized rather than recovered from a
    /// sidecar, timestamps saturated at a format boundary, a source
    /// reader's own best-effort warnings, and similar — see the module
    /// docs' "what does not round-trip" section for the recurring ones.
    pub warnings: Vec<String>,
}

/// Converts `input` to `output`, inferring both formats from their
/// extensions (see [`detect_format`]) and dispatching to the matching
/// direction — see the module docs' table.
///
/// # Errors
///
/// [`RosbagError::UnknownExtension`]/[`RosbagError::UnsupportedExtension`]
/// if either path's format cannot be inferred,
/// [`RosbagError::McapWriteUnsupported`] for any `→ .mcap` pair,
/// [`RosbagError::UnsupportedConversion`] for `.mcap → .db3` or any
/// same-format pair, or whatever the chosen direction's own conversion
/// reports.
pub fn convert_bag(input: &Path, output: &Path) -> Result<ConversionReport, RosbagError> {
    let from = detect_format(input)?;
    let to = detect_format(output)?;
    match (from, to) {
        (BagFormat::Arec, BagFormat::Db3) => arec_to_db3::arec_to_db3(input, output),
        (BagFormat::Db3, BagFormat::Arec) => db3_to_arec::db3_to_arec(input, output),
        (BagFormat::Mcap, BagFormat::Arec) => mcap_to_arec::mcap_to_arec(input, output),
        (_, BagFormat::Mcap) => Err(RosbagError::McapWriteUnsupported),
        _ => Err(RosbagError::UnsupportedConversion {
            from: from.extension(),
            to: to.extension(),
        }),
    }
}

// ---------------------------------------------------------------------
// Helpers shared by more than one direction module.
// ---------------------------------------------------------------------

/// Peeks `payload`'s first four bytes for a valid CDR encapsulation header
/// ([`astrs_cdr::EncapsulationHeader::from_bytes`]) — never a full decode.
/// Returns `"cdr"` when one parses, [`crate::RAW_SERIALIZATION_FORMAT`]
/// otherwise (including an empty or too-short payload).
#[must_use]
pub(crate) fn infer_serialization_format(payload: &[u8]) -> &'static str {
    if astrs_cdr::EncapsulationHeader::from_bytes(payload).is_ok() {
        "cdr"
    } else {
        crate::RAW_SERIALIZATION_FORMAT
    }
}

/// Converts an `.arec` [`astrs_time::HlcTimestamp`]'s physical-nanosecond
/// component to a `.db3`-shaped signed 64-bit timestamp, saturating at
/// [`i64::MAX`] — [`astrs_time::HlcTimestamp::physical_ns`] can represent
/// nanoseconds well past where `i64` can. Returns whether it actually
/// saturated, so a caller can count and report it rather than saturating
/// silently.
#[must_use]
pub(crate) fn physical_ns_to_i64(physical_ns: u64) -> (i64, bool) {
    i64::try_from(physical_ns).map_or((i64::MAX, true), |value| (value, false))
}

/// The inverse of [`physical_ns_to_i64`]: a `.db3` row's signed
/// nanosecond `timestamp` back to an unsigned physical nanosecond count,
/// saturating a pre-epoch (negative) value to zero — legal in `.db3`
/// (rosbag2 does not forbid it) but not representable in
/// [`astrs_time::HlcTimestamp`]. Returns whether it actually saturated.
#[must_use]
pub(crate) fn i64_to_physical_ns(timestamp_ns: i64) -> (u64, bool) {
    u64::try_from(timestamp_ns).map_or((0, true), |value| (value, false))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detect_format_recognizes_every_supported_extension_case_insensitively() {
        assert_eq!(detect_format(Path::new("x.arec")).unwrap(), BagFormat::Arec);
        assert_eq!(detect_format(Path::new("x.DB3")).unwrap(), BagFormat::Db3);
        assert_eq!(detect_format(Path::new("x.McAp")).unwrap(), BagFormat::Mcap);
    }

    #[test]
    fn detect_format_reports_typed_errors() {
        assert!(matches!(
            detect_format(Path::new("no-extension")),
            Err(RosbagError::UnknownExtension { .. })
        ));
        assert!(matches!(
            detect_format(Path::new("x.bag")),
            Err(RosbagError::UnsupportedExtension { .. })
        ));
    }

    #[test]
    fn every_format_extension_round_trips_through_detect_format() {
        for format in [BagFormat::Arec, BagFormat::Db3, BagFormat::Mcap] {
            let path = PathBuf::from(format!("x.{}", format.extension()));
            assert_eq!(detect_format(&path).unwrap(), format);
        }
    }

    #[test]
    fn convert_bag_refuses_arec_to_mcap_with_the_dedicated_error() {
        let err = convert_bag(Path::new("x.arec"), Path::new("y.mcap")).unwrap_err();
        assert!(matches!(err, RosbagError::McapWriteUnsupported));
    }

    #[test]
    fn convert_bag_refuses_mcap_to_db3_naming_the_arec_workaround() {
        let err = convert_bag(Path::new("x.mcap"), Path::new("y.db3")).unwrap_err();
        match err {
            RosbagError::UnsupportedConversion { from, to } => {
                assert_eq!(from, "mcap");
                assert_eq!(to, "db3");
            }
            other => panic!("expected UnsupportedConversion, got {other:?}"),
        }
    }

    #[test]
    fn convert_bag_refuses_same_format_pairs() {
        let err = convert_bag(Path::new("x.db3"), Path::new("y.db3")).unwrap_err();
        assert!(matches!(err, RosbagError::UnsupportedConversion { .. }));
    }

    #[test]
    fn convert_bag_propagates_a_bad_input_extension_before_touching_the_filesystem() {
        // Neither path exists on disk — an extension-shaped rejection
        // must win before any I/O is attempted.
        let err = convert_bag(Path::new("x.unknown"), Path::new("y.arec")).unwrap_err();
        assert!(matches!(err, RosbagError::UnsupportedExtension { .. }));
    }

    #[test]
    fn infer_serialization_format_recognizes_a_real_cdr_header() {
        // `CDR_LE`'s two-octet identifier (0x0001) big-endian, then a
        // zero options word — the smallest legal encapsulation header.
        let payload = [0x00, 0x01, 0x00, 0x00, 0xaa, 0xbb];
        assert_eq!(infer_serialization_format(&payload), "cdr");
    }

    #[test]
    fn infer_serialization_format_falls_back_to_raw_for_garbage_or_short_payloads() {
        assert_eq!(
            infer_serialization_format(&[0xff, 0xff, 0xff, 0xff]),
            crate::RAW_SERIALIZATION_FORMAT
        );
        assert_eq!(
            infer_serialization_format(&[]),
            crate::RAW_SERIALIZATION_FORMAT
        );
        assert_eq!(
            infer_serialization_format(&[0x00, 0x01]),
            crate::RAW_SERIALIZATION_FORMAT
        );
    }

    #[test]
    fn physical_ns_to_i64_is_identity_within_range() {
        assert_eq!(physical_ns_to_i64(0), (0, false));
        assert_eq!(physical_ns_to_i64(1_000), (1_000, false));
        assert_eq!(physical_ns_to_i64(i64::MAX as u64), (i64::MAX, false));
    }

    #[test]
    fn physical_ns_to_i64_saturates_and_reports_it_above_i64_max() {
        assert_eq!(physical_ns_to_i64(u64::MAX), (i64::MAX, true));
        assert_eq!(physical_ns_to_i64(i64::MAX as u64 + 1), (i64::MAX, true));
    }

    #[test]
    fn i64_to_physical_ns_is_identity_for_non_negative_values() {
        assert_eq!(i64_to_physical_ns(0), (0, false));
        assert_eq!(i64_to_physical_ns(1_000), (1_000, false));
        assert_eq!(i64_to_physical_ns(i64::MAX), (i64::MAX as u64, false));
    }

    #[test]
    fn i64_to_physical_ns_saturates_and_reports_it_for_negative_values() {
        assert_eq!(i64_to_physical_ns(-1), (0, true));
        assert_eq!(i64_to_physical_ns(i64::MIN), (0, true));
    }

    #[test]
    fn the_two_timestamp_helpers_round_trip_for_every_in_range_value() {
        for ns in [0u64, 1, 999, 1_000_000, i64::MAX as u64] {
            let (as_i64, saturated) = physical_ns_to_i64(ns);
            assert!(!saturated);
            let (back, saturated_back) = i64_to_physical_ns(as_i64);
            assert!(!saturated_back);
            assert_eq!(back, ns);
        }
    }

    // -----------------------------------------------------------------
    // `convert_bag`'s own dispatch, exercised end to end for every real
    // direction — proving the *dispatcher's* match arms wire up to the
    // right function, which each direction's own test module (testing
    // that function directly) does not by itself cover.
    // -----------------------------------------------------------------

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "astrs-rosbag-convert-dispatch-test-{}-{}-{label}",
            std::process::id(),
            uniq()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn convert_bag_dispatches_arec_to_db3() {
        let dir = temp_dir("arec-to-db3");
        let input = dir.join("session.arec");
        let output = dir.join("session.db3");
        let options = astrs_recording::WriterOptions::new(
            astrs_wire::DataflowId::from_u128(1),
            astrs_time::HlcTimestamp::new(1, 0),
        );
        let mut writer = astrs_recording::Writer::create(&input, options).unwrap();
        writer
            .append_parts(
                astrs_wire::NodeId::new("x").unwrap(),
                astrs_wire::DataId::new("y").unwrap(),
                astrs_wire::Metadata::new(astrs_time::HlcTimestamp::new(1, 0)),
                vec![1, 2, 3],
            )
            .unwrap();
        writer.finish().unwrap();

        let report = convert_bag(&input, &output).unwrap();
        assert_eq!(report.direction, "arec -> db3");
        assert_eq!(report.messages, 1);
        assert!(output.exists());
    }

    #[test]
    fn convert_bag_dispatches_db3_to_arec() {
        let dir = temp_dir("db3-to-arec");
        let input = dir.join("session.db3");
        let output = dir.join("session.arec");
        let mut writer = crate::db3::Writer::create(&input, "jazzy").unwrap();
        let topic_id = writer
            .create_topic(&crate::TopicRecord::new("/x", "", "cdr"))
            .unwrap();
        writer
            .write_messages(std::iter::once(Ok((topic_id, 1, vec![4, 5, 6]))))
            .unwrap();
        writer.finish().unwrap();

        let report = convert_bag(&input, &output).unwrap();
        assert_eq!(report.direction, "db3 -> arec");
        assert_eq!(report.messages, 1);
        assert!(output.exists());
    }

    #[test]
    fn convert_bag_dispatches_mcap_to_arec() {
        let dir = temp_dir("mcap-to-arec");
        let input = dir.join("session.mcap");
        let output = dir.join("session.arec");

        fn record(opcode: u8, payload: &[u8]) -> Vec<u8> {
            let mut out = vec![opcode];
            out.extend((payload.len() as u64).to_le_bytes());
            out.extend_from_slice(payload);
            out
        }
        fn prefixed(s: &str) -> Vec<u8> {
            let mut out = (s.len() as u32).to_le_bytes().to_vec();
            out.extend_from_slice(s.as_bytes());
            out
        }
        const MAGIC: [u8; 8] = [0x89, b'M', b'C', b'A', b'P', 0x30, b'\r', b'\n'];
        let mut bytes = MAGIC.to_vec();
        bytes.extend(record(
            0x01,
            &[0u32.to_le_bytes(), 0u32.to_le_bytes()].concat(),
        ));
        let mut channel_body = 1u16.to_le_bytes().to_vec();
        channel_body.extend(0u16.to_le_bytes());
        channel_body.extend(prefixed("/scan"));
        channel_body.extend(prefixed("cdr"));
        channel_body.extend(0u32.to_le_bytes());
        bytes.extend(record(0x04, &channel_body));
        let mut message_body = 1u16.to_le_bytes().to_vec();
        message_body.extend(0u32.to_le_bytes());
        message_body.extend(1u64.to_le_bytes());
        message_body.extend(1u64.to_le_bytes());
        message_body.extend_from_slice(&[7, 8, 9]);
        bytes.extend(record(0x05, &message_body));
        let footer_body = [
            0u64.to_le_bytes().to_vec(),
            0u64.to_le_bytes().to_vec(),
            0u32.to_le_bytes().to_vec(),
        ]
        .concat();
        bytes.extend(record(0x02, &footer_body));
        bytes.extend(MAGIC);
        std::fs::write(&input, bytes).unwrap();

        let report = convert_bag(&input, &output).unwrap();
        assert_eq!(report.direction, "mcap -> arec");
        assert_eq!(report.messages, 1);
        assert!(output.exists());
    }

    #[test]
    fn convert_bag_propagates_the_chosen_directions_own_error() {
        // Both extensions are valid and name a real direction (`.db3 ->
        // .arec`), but `input` itself does not exist — proving
        // `convert_bag` propagates whatever `db3_to_arec` reports rather
        // than swallowing or rewrapping it, the same way the dispatch
        // arms for the other two directions already implicitly do.
        let dir = temp_dir("propagates-inner-error");
        let input = dir.join("does-not-exist.db3");
        let output = dir.join("session.arec");
        let err = convert_bag(&input, &output).unwrap_err();
        assert!(matches!(err, RosbagError::Io { .. }), "{err:?}");
    }
}
