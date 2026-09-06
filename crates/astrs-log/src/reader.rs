//! Reads a line-oriented JSON log file back into [`LogRecord`]s.
//!
//! The connective tissue between [`crate::rotate::RotatingWriter`] (which
//! writes these files) and [`crate::merge::LogMerger`] (which needs
//! `Iterator<Item = LogRecord>` sources to merge) — this is how `astrs
//! logs -f` turns a daemon's on-disk rotated files into a mergeable
//! stream.

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::error::{LogError, Result};
use crate::record::LogRecord;

/// Iterates the [`LogRecord`]s in a line-oriented JSON log file, in file
/// order (which is HLC order for any file [`crate::rotate::RotatingWriter`]
/// produced, since it only ever appends).
///
/// Blank lines are skipped silently (rotation never writes one, but a
/// hand-edited or truncated file might). A line that is not valid
/// `LogRecord` JSON yields `Some(Err(LogError::Decode { .. }))` for that
/// line and then resumes from the next line — one malformed line does not
/// abort the rest of the file.
///
/// # Examples
///
/// ```
/// use astrs_log::{HlcTimestamp, LogFileReader, LogLevel, LogRecord, RotatingWriter, RotationConfig};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let mut dir = std::env::temp_dir();
/// dir.push(format!("astrs-log-reader-doctest-{}", std::process::id()));
/// std::fs::create_dir_all(&dir)?;
/// let path = dir.join("daemon.log");
///
/// let writer = RotatingWriter::open(&path, RotationConfig::default())?;
/// writer.write_record(&LogRecord::new(HlcTimestamp::new(0, 0), LogLevel::Info, "t", "hello"))?;
///
/// let records: Vec<LogRecord> = LogFileReader::open(&path)?.collect::<Result<_, _>>()?;
/// assert_eq!(records.len(), 1);
/// assert_eq!(records[0].message, "hello");
///
/// std::fs::remove_dir_all(&dir).ok();
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct LogFileReader {
    lines: io::Lines<BufReader<File>>,
    path: PathBuf,
    line_no: u64,
}

impl LogFileReader {
    /// Opens `path` for reading.
    ///
    /// # Errors
    ///
    /// Returns [`LogError::Io`] if the file cannot be opened.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let file = File::open(&path).map_err(|source| LogError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(Self {
            lines: BufReader::new(file).lines(),
            path,
            line_no: 0,
        })
    }

    /// The file this reader is reading.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The 1-based number of the most recently yielded line (`0` before
    /// the first call to `next`).
    #[must_use]
    pub const fn line_number(&self) -> u64 {
        self.line_no
    }
}

impl Iterator for LogFileReader {
    type Item = Result<LogRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let raw = self.lines.next()?;
            self.line_no += 1;
            let line = match raw {
                Ok(line) => line,
                Err(source) => {
                    return Some(Err(LogError::Io {
                        path: self.path.clone(),
                        source,
                    }));
                }
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parsed = serde_json::from_str(trimmed).map_err(|source| LogError::Decode {
                path: self.path.clone(),
                line: self.line_no,
                source,
            });
            return Some(parsed);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::level::LogLevel;
    use crate::merge::merge_logs;
    use crate::rotate::{RotatingWriter, RotationConfig, rotated_path};
    use crate::test_util::unique_temp_dir;
    use astrs_time::HlcTimestamp;
    use std::fs;

    fn rec(seq: u64) -> LogRecord {
        LogRecord::new(HlcTimestamp::new(seq, 0), LogLevel::Info, "t", "m").with_seq(seq)
    }

    #[test]
    fn reads_back_what_the_writer_wrote_in_order() {
        let dir = unique_temp_dir("reader-basic");
        let path = dir.join("log.jsonl");
        let writer = RotatingWriter::open(&path, RotationConfig::default()).unwrap();
        for i in 0..5u64 {
            writer.write_record(&rec(i)).unwrap();
        }
        let read: Vec<LogRecord> = LogFileReader::open(&path)
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(read.len(), 5);
        for (i, record) in read.iter().enumerate() {
            assert_eq!(record.seq, i as u64);
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn skips_blank_lines() {
        let dir = unique_temp_dir("reader-blank");
        let path = dir.join("log.jsonl");
        fs::write(&path, "\n{\"hlc\":{\"physical_ns\":0,\"logical\":0},\"level\":\"info\",\"target\":\"t\",\"message\":\"m\"}\n\n").unwrap();
        let read: Vec<LogRecord> = LogFileReader::open(&path)
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(read.len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_line_yields_a_decode_error_with_line_number_and_resumes() {
        let dir = unique_temp_dir("reader-malformed");
        let path = dir.join("log.jsonl");
        let good =
            r#"{"hlc":{"physical_ns":0,"logical":0},"level":"info","target":"t","message":"m"}"#;
        fs::write(&path, format!("{good}\nnot json\n{good}\n")).unwrap();
        let results: Vec<Result<LogRecord>> = LogFileReader::open(&path).unwrap().collect();
        assert_eq!(results.len(), 3);
        assert!(results[0].is_ok());
        match &results[1] {
            Err(LogError::Decode { line, .. }) => assert_eq!(*line, 2),
            other => panic!("expected a Decode error at line 2, got {other:?}"),
        }
        assert!(
            results[2].is_ok(),
            "the reader must resume after a malformed line"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_missing_file_returns_an_io_error() {
        let dir = unique_temp_dir("reader-missing");
        let err = LogFileReader::open(dir.join("nope.jsonl")).unwrap_err();
        assert!(matches!(err, LogError::Io { .. }));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reading_rotated_files_and_merging_reconstructs_full_history() {
        let dir = unique_temp_dir("reader-merge-integration");
        let path = dir.join("log.jsonl");
        // Each `rec(_)` line is 143 bytes; a 100-byte cap means every
        // write after the first rotates, so 8 writes produce 7 archives.
        // `max_rotated_files: 10` retains all of them -- retention
        // evicting older archives is correct, tested behavior (see
        // `rotate::tests::retention_drops_the_oldest_archive`), just not
        // what this test is about.
        let config = RotationConfig {
            max_log_size: 100,
            max_rotated_files: 10,
            ..RotationConfig::default()
        };
        let writer = RotatingWriter::open(&path, config).unwrap();
        for i in 0..8u64 {
            writer.write_record(&rec(i)).unwrap();
        }
        assert!(
            rotated_path(&path, 1).exists(),
            "test setup should have produced at least one archive"
        );

        // Every rotated file plus the active file, each individually
        // ascending, fed through the merger: exactly what `astrs logs -f`
        // does per daemon (one file set) before merging across daemons.
        let mut sources: Vec<Vec<LogRecord>> = Vec::new();
        for idx in (1..=10).rev() {
            let archive = rotated_path(&path, idx);
            if archive.exists() {
                let records: Vec<LogRecord> = LogFileReader::open(&archive)
                    .unwrap()
                    .collect::<Result<_>>()
                    .unwrap();
                sources.push(records);
            }
        }
        sources.push(
            LogFileReader::open(&path)
                .unwrap()
                .collect::<Result<_>>()
                .unwrap(),
        );

        let merged: Vec<u64> = merge_logs(sources.into_iter().map(Vec::into_iter).collect())
            .map(|r| r.seq)
            .collect();
        assert_eq!(merged, (0..8).collect::<Vec<_>>());
        fs::remove_dir_all(&dir).ok();
    }

    /// End-to-end check that this crate's `hlc` field genuinely is the
    /// timestamp type `astrs-time` issues, not merely a same-shaped
    /// stand-in: a real [`astrs_time::HlcClock`] stamps records, they are
    /// written, read back, and re-merged, and every step agrees.
    #[test]
    fn hlc_clock_stamped_records_round_trip_through_write_read_and_merge() {
        use astrs_time::{HlcClock, ManualClock};

        let dir = unique_temp_dir("reader-hlc-clock-integration");
        let path = dir.join("log.jsonl");
        let writer = RotatingWriter::open(&path, RotationConfig::default()).unwrap();

        // A manual clock held fixed: every `now()` call falls back to the
        // HLC "same physical instant" branch, so successive records are
        // ordered by the clock's own logical-counter bump alone -- the
        // exact scenario `format::human`'s `+<logical>` suffix exists for.
        let clock = HlcClock::new(ManualClock::new(1_700_000_000_000_000_000));
        let mut records = Vec::new();
        for i in 0..5u64 {
            let stamped = clock.stamp(());
            records.push(
                LogRecord::new(stamped.ts, LogLevel::Info, "t", format!("event {i}")).with_seq(i),
            );
        }
        // The clock's send-rule guarantees strictly increasing timestamps
        // even with a frozen wall clock.
        for pair in records.windows(2) {
            assert!(pair[0].hlc < pair[1].hlc, "HlcClock must be monotone");
        }
        for record in &records {
            writer.write_record(record).unwrap();
        }

        let read: Vec<LogRecord> = LogFileReader::open(&path)
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(read, records);

        // A single already-sorted source "merges" back to itself.
        let merged: Vec<LogRecord> = merge_logs(vec![read.into_iter()]).collect();
        assert_eq!(merged, records);

        fs::remove_dir_all(&dir).ok();
    }
}
