//! [`Reader`] — opens an existing rosbag2 `.db3` (blueprint §10.6),
//! tolerating every schema version rosbag2 has shipped (see [`schema`
//! docs](crate::db3::schema)), and its `metadata.yaml` sidecar when
//! present — falling back to the in-database `metadata` table's own
//! duplicate (schema version 3+) when the external file is missing or
//! will not parse, before finally giving up and reporting a warning.
//!
//! # Bounded memory
//!
//! [`Reader::iter_messages`] never loads the whole `messages` table at
//! once: `oxisql-core`'s [`Connection::query`](oxisql_core::Connection::query)
//! is fully eager (there is no lazy row-cursor in the version this crate
//! pins — §19.1), so [`MessageIter`] pages through `messages` in ordered
//! `id`-range batches of [`DEFAULT_BATCH_SIZE`] instead, keeping peak
//! memory proportional to one batch's payload bytes rather than the whole
//! bag's.

use std::path::{Path, PathBuf};

use oxisql_core::Value;
use oxisql_sqlite_compat::blocking::SqliteConnectionBlocking;

use crate::db3::metadata_yaml::BagMetadata;
use crate::db3::schema;
use crate::db3::writer::metadata_path_for;
use crate::error::RosbagError;
use crate::topic::{TopicRecord, parse_qos_profiles_yaml};

/// How many `messages` rows [`MessageIter`] fetches per round trip to the
/// database.
pub const DEFAULT_BATCH_SIZE: i64 = 1_000;

/// One row of the `messages` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The row's own id (the pagination cursor
    /// [`MessageIter`] advances by).
    pub id: i64,
    /// The `topics.id` this message was published on.
    pub topic_id: i64,
    /// The raw rosbag2 timestamp, nanoseconds since the Unix epoch —
    /// signed, so a (rare) pre-epoch value round-trips exactly. See
    /// [`crate::convert`]'s docs for how this maps to and from `.arec`'s
    /// unsigned [`astrs_time::HlcTimestamp`].
    pub timestamp_ns: i64,
    /// The serialized message bytes, exactly as stored — this crate never
    /// decodes them (blueprint §10.6: "CDR payloads pass through
    /// opaquely").
    pub data: Vec<u8>,
}

/// An open `.db3`, ready to enumerate topics and stream messages.
///
/// # Examples
///
/// ```
/// use astrs_rosbag::TopicRecord;
/// use astrs_rosbag::db3::{Reader, Writer};
///
/// let dir = std::env::temp_dir().join(format!("astrs-rosbag-reader-doctest-{}", std::process::id()));
/// std::fs::create_dir_all(&dir)?;
/// let path = dir.join("session.db3");
///
/// let mut writer = Writer::create(&path, "jazzy")?;
/// let topic_id = writer.create_topic(&TopicRecord::new("/scan", "sensor_msgs/msg/LaserScan", "cdr"))?;
/// writer.write_messages(std::iter::once(Ok((topic_id, 1_000, vec![1, 2, 3]))))?;
/// writer.finish()?;
///
/// let reader = Reader::open(&path)?;
/// assert_eq!(reader.topics().len(), 1);
/// let messages: Vec<_> = reader.iter_messages().collect::<Result<_, _>>()?;
/// assert_eq!(messages.len(), 1);
/// assert_eq!(messages[0].data, vec![1, 2, 3]);
/// # std::fs::remove_dir_all(&dir).ok();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct Reader {
    conn: SqliteConnectionBlocking,
    path: PathBuf,
    schema_version: u32,
    /// `(id, record)`, in `topics.id` order.
    topics: Vec<(i64, TopicRecord)>,
    metadata: Option<BagMetadata>,
    warnings: Vec<String>,
}

impl Reader {
    /// Opens `path`, reading its topic table (tolerating every schema
    /// version — see [`schema`]'s docs) and, if
    /// present next to it, its `metadata.yaml` sidecar.
    ///
    /// A missing or unparseable `metadata.yaml` is never fatal: it
    /// degrades to [`Reader::metadata`] returning `None` and a note in
    /// [`Reader::warnings`], since every field this crate actually needs
    /// to convert a bag (topics, messages) lives in the database itself.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Io`] if `path` does not exist or cannot be opened,
    /// [`RosbagError::MissingTable`] if it has no `topics` table (not a
    /// rosbag2 database), or [`RosbagError::Sql`]/
    /// [`RosbagError::ColumnTypeMismatch`] for other structural problems.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RosbagError> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Err(RosbagError::io(
                &path,
                std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
            ));
        }
        let path_str = path.to_str().ok_or_else(|| {
            RosbagError::Internal(format!("`{}` is not valid UTF-8", path.display()))
        })?;
        let conn = SqliteConnectionBlocking::open(path_str)
            .map_err(|source| RosbagError::sql(&path, source))?;

        let schema_version = schema::detect_schema_version(&conn, &path)?;
        let has_qos = schema::column_exists(&conn, &path, "topics", "offered_qos_profiles")?;
        let has_hash = schema::column_exists(&conn, &path, "topics", "type_description_hash")?;

        let mut warnings = Vec::new();
        let topics = read_topics(&conn, &path, has_qos, has_hash, &mut warnings)?;

        let metadata_path = metadata_path_for(&path);
        let file_result = if metadata_path.exists() {
            Some(
                std::fs::read_to_string(&metadata_path)
                    .map_err(|source| RosbagError::io(&metadata_path, source))
                    .and_then(|yaml| {
                        BagMetadata::from_yaml_str(&yaml).map_err(|source| {
                            RosbagError::Internal(format!(
                                "`{}` did not parse: {source}",
                                metadata_path.display()
                            ))
                        })
                    }),
            )
        } else {
            None
        };
        // The external file is preferred (it is what real `ros2 bag info`
        // reads), but a missing or unusable one falls back to the
        // in-database `metadata` table before giving up — the redundancy
        // `Writer::finish` (this crate's own writer) always populates, and
        // real rosbag2 keeps too (`schema`'s module docs).
        let metadata = match file_result {
            Some(Ok(metadata)) => Some(metadata),
            Some(Err(err)) => metadata_or_warn(
                read_metadata_from_table(&conn, &path),
                || {
                    format!(
                        "metadata.yaml at `{}` was not usable ({err}), and the in-database \
                         `metadata` table had no usable copy either, so per-file/per-topic \
                         summary statistics fall back to the database alone",
                        metadata_path.display()
                    )
                },
                &mut warnings,
            ),
            None => metadata_or_warn(
                read_metadata_from_table(&conn, &path),
                || {
                    format!(
                        "no `metadata.yaml` at `{}`, and the in-database `metadata` table had \
                         no usable copy either, so per-file/per-topic summary statistics fall \
                         back to the database alone",
                        metadata_path.display()
                    )
                },
                &mut warnings,
            ),
        };

        Ok(Self {
            conn,
            path,
            schema_version,
            topics,
            metadata,
            warnings,
        })
    }

    /// The `.db3` path this reader opened.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The schema version [`schema::detect_schema_version`] found (1..4).
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Every topic, as `(row id, record)`, in `topics.id` order.
    #[must_use]
    pub fn topics(&self) -> &[(i64, TopicRecord)] {
        &self.topics
    }

    /// Looks up a topic by its row id.
    #[must_use]
    pub fn topic_by_id(&self, id: i64) -> Option<&TopicRecord> {
        self.topics
            .iter()
            .find(|(topic_id, _)| *topic_id == id)
            .map(|(_, record)| record)
    }

    /// The parsed `metadata.yaml` sidecar — or, if that file was absent or
    /// unusable, its in-database fallback (see the module docs) — if
    /// either was present and usable.
    #[must_use]
    pub fn metadata(&self) -> Option<&BagMetadata> {
        self.metadata.as_ref()
    }

    /// Non-fatal notes accumulated while opening — an unusable
    /// `metadata.yaml`, a `topics.offered_qos_profiles` value this crate
    /// could not parse, and similar best-effort degradations.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The exact number of rows in `messages`, via `SELECT COUNT(*)`
    /// (never estimated from `metadata.yaml`, which may be stale or
    /// absent).
    ///
    /// # Errors
    ///
    /// [`RosbagError::Sql`] if the query fails, or
    /// [`RosbagError::ColumnTypeMismatch`] if the engine somehow returns a
    /// non-integer count.
    pub fn message_count(&self) -> Result<u64, RosbagError> {
        let rows = self
            .conn
            .query("SELECT COUNT(*) FROM messages", &[])
            .map_err(|source| RosbagError::sql(&self.path, source))?;
        match rows.first().and_then(|row| row.get_by_index(0)) {
            Some(Value::I64(count)) => Ok(u64::try_from(*count).unwrap_or(0)),
            other => Err(RosbagError::ColumnTypeMismatch {
                path: self.path.clone(),
                table: "messages",
                column: "COUNT(*)",
                expected: "integer",
                found: value_type_name(other),
                row: -1,
            }),
        }
    }

    /// Iterates every message, in `messages.id` order (rosbag2's own
    /// insertion order — not necessarily timestamp order for a bag some
    /// other tool wrote out of order), at [`DEFAULT_BATCH_SIZE`].
    #[must_use]
    pub fn iter_messages(&self) -> MessageIter<'_> {
        self.iter_messages_with_batch_size(DEFAULT_BATCH_SIZE)
    }

    /// As [`Reader::iter_messages`], with an explicit fetch batch size
    /// (clamped to at least 1).
    #[must_use]
    pub fn iter_messages_with_batch_size(&self, batch_size: i64) -> MessageIter<'_> {
        MessageIter {
            conn: &self.conn,
            path: &self.path,
            batch: Vec::new().into_iter(),
            last_id: 0,
            exhausted: false,
            batch_size: batch_size.max(1),
        }
    }
}

/// Best-effort read of the in-database `metadata` table's most recent row
/// (present from schema version 3 onward — see [`schema`]'s
/// module docs), used by [`Reader::open`] as its fallback when
/// `metadata.yaml` is absent or unusable.
///
/// `None` for any reason at all — no `metadata` table, no rows, or a row
/// whose `metadata` column is not text or does not parse as
/// [`BagMetadata`] — the caller folds every one of those into the same
/// "metadata not usable" warning the external-file path already reports,
/// so a foreign `.db3` never earns two overlapping warnings for one
/// underlying "no usable metadata" fact.
fn read_metadata_from_table(conn: &SqliteConnectionBlocking, path: &Path) -> Option<BagMetadata> {
    if !schema::table_exists(conn, path, "metadata").ok()? {
        return None;
    }
    let rows = conn
        .query(
            "SELECT metadata FROM metadata ORDER BY id DESC LIMIT 1",
            &[],
        )
        .ok()?;
    let Value::Text(yaml) = rows.first()?.get_by_index(0)? else {
        return None;
    };
    BagMetadata::from_yaml_str(yaml).ok()
}

/// Returns `fallback` if it is `Some`; otherwise pushes `reason()` onto
/// `warnings` and returns `None`. The one place [`Reader::open`]'s two
/// (file-then-table) metadata fallback branches converge, so both report
/// the "nothing usable was found" case identically.
fn metadata_or_warn(
    fallback: Option<BagMetadata>,
    reason: impl FnOnce() -> String,
    warnings: &mut Vec<String>,
) -> Option<BagMetadata> {
    if fallback.is_some() {
        return fallback;
    }
    warnings.push(reason());
    None
}

fn value_type_name(value: Option<&Value>) -> &'static str {
    match value {
        None => "absent",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "bool",
        Some(Value::I64(_)) => "integer",
        Some(Value::F64(_)) => "float",
        Some(Value::Text(_)) => "text",
        Some(Value::Blob(_)) => "blob",
        Some(_) => "other",
    }
}

/// Reads `topics`, adapting the column list to what actually exists —
/// mirroring `SqliteStorage::fill_topics_and_types()`'s three-tier
/// fallback rather than trusting `schema_version` alone (see
/// [`schema`]'s module docs).
fn read_topics(
    conn: &SqliteConnectionBlocking,
    path: &Path,
    has_qos: bool,
    has_hash: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<(i64, TopicRecord)>, RosbagError> {
    let select = match (has_qos, has_hash) {
        (true, true) => {
            "SELECT id, name, type, serialization_format, offered_qos_profiles, \
             type_description_hash FROM topics ORDER BY id"
        }
        (true, false) => {
            "SELECT id, name, type, serialization_format, offered_qos_profiles FROM topics \
             ORDER BY id"
        }
        (false, _) => "SELECT id, name, type, serialization_format FROM topics ORDER BY id",
    };
    let rows = conn
        .query(select, &[])
        .map_err(|source| RosbagError::sql(path, source))?;

    let mut topics = Vec::with_capacity(rows.len());
    for row in &rows {
        let id = match row.get_by_index(0) {
            Some(Value::I64(id)) => *id,
            other => {
                return Err(RosbagError::ColumnTypeMismatch {
                    path: path.to_path_buf(),
                    table: "topics",
                    column: "id",
                    expected: "integer",
                    found: value_type_name(other),
                    row: -1,
                });
            }
        };
        let name = text_column(row, 1, "topics", "name", id, path)?;
        let r#type = text_column(row, 2, "topics", "type", id, path)?;
        let serialization_format = text_column(row, 3, "topics", "serialization_format", id, path)?;
        let offered_qos_profiles = if has_qos {
            let raw = text_column(row, 4, "topics", "offered_qos_profiles", id, path)?;
            match parse_qos_profiles_yaml(&raw) {
                Some(profiles) => profiles,
                None => {
                    warnings.push(format!(
                        "topic `{name}` (id {id}): `offered_qos_profiles` was not the YAML \
                         shape this crate writes; its QoS profile list was left empty"
                    ));
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let type_description_hash = if has_hash {
            text_column(row, 5, "topics", "type_description_hash", id, path)?
        } else {
            String::new()
        };
        topics.push((
            id,
            TopicRecord {
                topic: name,
                r#type,
                serialization_format,
                offered_qos_profiles,
                type_description_hash,
            },
        ));
    }
    Ok(topics)
}

fn text_column(
    row: &oxisql_core::Row,
    index: usize,
    table: &'static str,
    column: &'static str,
    row_id: i64,
    path: &Path,
) -> Result<String, RosbagError> {
    match row.get_by_index(index) {
        Some(Value::Text(text)) => Ok(text.clone()),
        other => Err(RosbagError::ColumnTypeMismatch {
            path: path.to_path_buf(),
            table,
            column,
            expected: "text",
            found: value_type_name(other),
            row: row_id,
        }),
    }
}

fn row_to_message(row: &oxisql_core::Row, path: &Path) -> Result<Message, RosbagError> {
    let id = match row.get_by_index(0) {
        Some(Value::I64(id)) => *id,
        other => {
            return Err(RosbagError::ColumnTypeMismatch {
                path: path.to_path_buf(),
                table: "messages",
                column: "id",
                expected: "integer",
                found: value_type_name(other),
                row: -1,
            });
        }
    };
    let topic_id = match row.get_by_index(1) {
        Some(Value::I64(topic_id)) => *topic_id,
        other => {
            return Err(RosbagError::ColumnTypeMismatch {
                path: path.to_path_buf(),
                table: "messages",
                column: "topic_id",
                expected: "integer",
                found: value_type_name(other),
                row: id,
            });
        }
    };
    let timestamp_ns = match row.get_by_index(2) {
        Some(Value::I64(timestamp)) => *timestamp,
        other => {
            return Err(RosbagError::ColumnTypeMismatch {
                path: path.to_path_buf(),
                table: "messages",
                column: "timestamp",
                expected: "integer",
                found: value_type_name(other),
                row: id,
            });
        }
    };
    let data = match row.get_by_index(3) {
        Some(Value::Blob(data)) => data.clone(),
        other => {
            return Err(RosbagError::ColumnTypeMismatch {
                path: path.to_path_buf(),
                table: "messages",
                column: "data",
                expected: "blob",
                found: value_type_name(other),
                row: id,
            });
        }
    };
    Ok(Message {
        id,
        topic_id,
        timestamp_ns,
        data,
    })
}

/// A lazy, batched iterator over a [`Reader`]'s `messages` table (see the
/// module docs' bounded-memory note). Obtain via
/// [`Reader::iter_messages`]/[`Reader::iter_messages_with_batch_size`].
pub struct MessageIter<'a> {
    conn: &'a SqliteConnectionBlocking,
    path: &'a Path,
    batch: std::vec::IntoIter<oxisql_core::Row>,
    last_id: i64,
    exhausted: bool,
    batch_size: i64,
}

impl Iterator for MessageIter<'_> {
    type Item = Result<Message, RosbagError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(row) = self.batch.next() {
                return Some(row_to_message(&row, self.path));
            }
            if self.exhausted {
                return None;
            }
            // `LIMIT` is interpolated as a literal integer rather than a
            // bound `$N` parameter: `oxisql-sqlite-compat` 0.4.1 (via
            // `oxisqlite`) silently returns zero rows for any statement
            // that binds *both* a `WHERE`-clause parameter and a
            // `LIMIT`-clause parameter together — confirmed by direct
            // experiment (isolating `WHERE $1`, `LIMIT $1`, `WHERE $1 AND
            // $2`, and `WHERE <literal> LIMIT $1` each individually
            // returns every row; only the `WHERE $N ... LIMIT $M`
            // combination, in either parameter order and with or without
            // `ORDER BY` between them, returns none). `self.batch_size`
            // is always an internally-constructed, `.max(1)`-clamped
            // `i64` (see `Reader::iter_messages_with_batch_size`), never
            // untrusted external text, so formatting it directly into the
            // SQL string carries no injection risk.
            let batch_size = self.batch_size;
            let rows = match self.conn.query(
                &format!(
                    "SELECT id, topic_id, timestamp, data FROM messages WHERE id > $1 ORDER BY \
                     id LIMIT {batch_size}"
                ),
                &[&self.last_id],
            ) {
                Ok(rows) => rows,
                Err(source) => {
                    self.exhausted = true;
                    return Some(Err(RosbagError::sql(self.path, source)));
                }
            };
            if rows.is_empty() {
                self.exhausted = true;
                return None;
            }
            if let Some(Value::I64(last)) = rows.last().and_then(|row| row.get_by_index(0)) {
                self.last_id = *last;
            } else {
                self.exhausted = true;
                return Some(Err(RosbagError::ColumnTypeMismatch {
                    path: self.path.to_path_buf(),
                    table: "messages",
                    column: "id",
                    expected: "integer",
                    found: "absent",
                    row: -1,
                }));
            }
            self.batch = rows.into_iter();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::db3::writer::Writer;

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn temp_path(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "astrs-rosbag-reader-test-{}-{}-{label}",
            std::process::id(),
            uniq()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("session.db3")
    }

    fn cleanup(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn write_sample(path: &Path, message_count: i64) -> i64 {
        let mut writer = Writer::create(path, "jazzy").unwrap();
        let topic_id = writer
            .create_topic(&TopicRecord::new(
                "/scan",
                "sensor_msgs/msg/LaserScan",
                "cdr",
            ))
            .unwrap();
        let rows = (0..message_count)
            .map(|i| Ok((topic_id, i * 10, vec![i as u8; 4])))
            .collect::<Vec<_>>();
        writer.write_messages(rows.into_iter()).unwrap();
        writer.finish().unwrap();
        topic_id
    }

    #[test]
    fn open_reads_topics_and_metadata_written_by_this_crates_own_writer() {
        let path = temp_path("round-trip");
        write_sample(&path, 3);
        let reader = Reader::open(&path).unwrap();
        assert_eq!(reader.schema_version(), schema::CURRENT_SCHEMA_VERSION);
        assert_eq!(reader.topics().len(), 1);
        assert_eq!(reader.topics()[0].1.topic, "/scan");
        assert!(reader.metadata().is_some());
        assert!(reader.warnings().is_empty(), "{:?}", reader.warnings());
        cleanup(&path);
    }

    #[test]
    fn iter_messages_yields_every_row_in_id_order() {
        let path = temp_path("iterate");
        write_sample(&path, 5);
        let reader = Reader::open(&path).unwrap();
        let messages: Vec<Message> = reader.iter_messages().collect::<Result<_, _>>().unwrap();
        assert_eq!(messages.len(), 5);
        let ids: Vec<i64> = messages.iter().map(|m| m.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "must already be in id order");
        assert_eq!(messages[2].timestamp_ns, 20);
        assert_eq!(messages[2].data, vec![2u8; 4]);
    }

    #[test]
    fn iter_messages_pages_across_multiple_batches() {
        let path = temp_path("paginate");
        write_sample(&path, 25);
        let reader = Reader::open(&path).unwrap();
        let messages: Vec<Message> = reader
            .iter_messages_with_batch_size(7)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(messages.len(), 25);
        assert_eq!(reader.message_count().unwrap(), 25);
        cleanup(&path);
    }

    #[test]
    fn an_empty_bag_iterates_nothing() {
        let path = temp_path("empty");
        write_sample(&path, 0);
        let reader = Reader::open(&path).unwrap();
        assert_eq!(reader.iter_messages().count(), 0);
        assert_eq!(reader.message_count().unwrap(), 0);
        cleanup(&path);
    }

    #[test]
    fn open_reports_a_typed_error_for_a_missing_file() {
        let path = std::env::temp_dir().join("astrs-rosbag-reader-does-not-exist.db3");
        let err = Reader::open(&path).unwrap_err();
        assert!(matches!(err, RosbagError::Io { .. }));
    }

    #[test]
    fn open_reports_missing_table_for_a_non_rosbag2_sqlite_file() {
        let path = temp_path("not-a-bag");
        let conn = SqliteConnectionBlocking::open(path.to_str().unwrap()).unwrap();
        conn.execute("CREATE TABLE unrelated(x INTEGER)", &[])
            .unwrap();
        drop(conn);
        let err = Reader::open(&path).unwrap_err();
        assert!(matches!(
            err,
            RosbagError::MissingTable {
                table: "topics",
                ..
            }
        ));
        cleanup(&path);
    }

    #[test]
    fn a_missing_metadata_yaml_recovers_from_the_in_database_table() {
        let path = temp_path("no-metadata-file");
        {
            let mut writer = Writer::create(&path, "jazzy").unwrap();
            writer
                .create_topic(&TopicRecord::new("/x", "", "cdr"))
                .unwrap();
            // Deliberately no `finish()`, and drop the writer's own
            // best-effort metadata.yaml via a fresh remove — simulating a
            // bag copied without its sidecar file. `Writer::finish` (run
            // via `Drop`, since this scope never calls it explicitly)
            // still wrote the in-database `metadata` table copy, which
            // `Reader::open` now falls back to.
        }
        let _ = std::fs::remove_file(metadata_path_for(&path));
        let reader = Reader::open(&path).unwrap();
        assert!(reader.warnings().is_empty(), "{:?}", reader.warnings());
        let metadata = reader.metadata().expect("recovered from the DB table");
        assert_eq!(
            metadata.topics_with_message_count[0].topic_metadata.name,
            "/x"
        );
        assert!(reader.topics().iter().any(|(_, t)| t.topic == "/x"));
        cleanup(&path);
    }

    #[test]
    fn no_metadata_anywhere_is_a_warning_not_an_error() {
        // A hand-built database at the current schema (so the `metadata`
        // table exists) with no row in it and no external file — the
        // genuine "nothing usable was found" case, distinct from the test
        // above where the in-database copy still saves the day.
        let path = temp_path("no-metadata-anywhere");
        let conn = SqliteConnectionBlocking::open(path.to_str().unwrap()).unwrap();
        for stmt in schema::CREATE_STATEMENTS {
            conn.execute(stmt, &[]).unwrap();
        }
        drop(conn);
        let reader = Reader::open(&path).unwrap();
        assert!(reader.metadata().is_none());
        assert_eq!(reader.warnings().len(), 1);
        assert!(
            reader.warnings()[0].contains("metadata"),
            "{:?}",
            reader.warnings()
        );
        cleanup(&path);
    }

    #[test]
    fn a_topic_with_no_qos_column_reads_as_schema_version_one() {
        let path = temp_path("v1");
        let conn = SqliteConnectionBlocking::open(path.to_str().unwrap()).unwrap();
        conn.execute(
            "CREATE TABLE topics(id INTEGER PRIMARY KEY, name TEXT NOT NULL, type TEXT NOT NULL, \
             serialization_format TEXT NOT NULL)",
            &[],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE messages(id INTEGER PRIMARY KEY, topic_id INTEGER NOT NULL, \
             timestamp INTEGER NOT NULL, data BLOB NOT NULL)",
            &[],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO topics (name, type, serialization_format) VALUES ($1, $2, $3)",
            &[&"/legacy", &"std_msgs/msg/String", &"cdr"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (topic_id, timestamp, data) VALUES ($1, $2, $3)",
            &[&1i64, &42i64, &vec![7u8]],
        )
        .unwrap();
        drop(conn);

        let reader = Reader::open(&path).unwrap();
        assert_eq!(reader.schema_version(), 1);
        assert_eq!(reader.topics().len(), 1);
        assert_eq!(reader.topics()[0].1.topic, "/legacy");
        assert!(reader.topics()[0].1.offered_qos_profiles.is_empty());
        let messages: Vec<Message> = reader.iter_messages().collect::<Result<_, _>>().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].data, vec![7u8]);
        cleanup(&path);
    }

    #[test]
    fn an_unparseable_qos_column_degrades_to_empty_with_a_warning() {
        let path = temp_path("bad-qos");
        let conn = SqliteConnectionBlocking::open(path.to_str().unwrap()).unwrap();
        conn.execute(schema::CREATE_TABLE_TOPICS, &[]).unwrap();
        conn.execute(schema::CREATE_TABLE_MESSAGES, &[]).unwrap();
        conn.execute(
            "INSERT INTO topics (id, name, type, serialization_format, offered_qos_profiles, \
             type_description_hash) VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                &1i64,
                &"/x",
                &"",
                &"cdr",
                &"not: [valid, yaml, for, our, shape",
                &"",
            ],
        )
        .unwrap();
        drop(conn);

        let reader = Reader::open(&path).unwrap();
        assert!(reader.topics()[0].1.offered_qos_profiles.is_empty());
        // This hand-built database has no `metadata` table at all (only
        // `topics`/`messages`, unlike `schema::CREATE_STATEMENTS`'s full
        // set), so `Reader::open`'s metadata fallback also warns —
        // `contains`, not an exact count, keeps this test about the QoS
        // column specifically rather than coupled to that unrelated warning.
        assert!(
            reader
                .warnings()
                .iter()
                .any(|warning| warning.contains("offered_qos_profiles")),
            "{:?}",
            reader.warnings()
        );
        cleanup(&path);
    }

    #[test]
    fn message_count_matches_iteration_length() {
        let path = temp_path("count");
        write_sample(&path, 12);
        let reader = Reader::open(&path).unwrap();
        assert_eq!(reader.message_count().unwrap(), 12);
        assert_eq!(reader.iter_messages().count(), 12);
        cleanup(&path);
    }
}
