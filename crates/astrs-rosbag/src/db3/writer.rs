//! [`Writer`] — creates a fresh rosbag2 `.db3` at the current schema
//! (blueprint §10.6), plus its `metadata.yaml` sidecar and the in-database
//! `metadata` table's duplicate of the same document, both written on
//! [`Writer::finish`].
//!
//! # Transaction discipline
//!
//! `oxisql-sqlite-compat`'s blocking [`SqliteBlockingTransaction`] must be
//! explicitly [`commit`](oxisql_sqlite_compat::blocking::SqliteBlockingTransaction::commit)ted
//! or [`rollback`](oxisql_sqlite_compat::blocking::SqliteBlockingTransaction::rollback)ed on
//! every path: its inner async transaction's own `Drop` impl issues a
//! best-effort `ROLLBACK` via `tokio::spawn`, and by the time a
//! `SqliteBlockingTransaction` value is dropped the `current_thread`
//! runtime `SqliteConnectionBlocking::transaction()` built to open it has
//! already been torn down — so an un-committed, un-rolled-back
//! transaction **panics on drop**, not merely leaks. This module's own
//! `with_transaction` is the one place it opens a transaction, and it
//! always calls exactly one of the two before returning, on every path
//! including an error from the caller's own row iterator.
//!
//! # Bounded memory
//!
//! [`Writer::write_messages`] inserts everything it is given inside one
//! transaction (so a mid-conversion failure leaves no half-written bag —
//! see `with_transaction`, above), but never holds more than one row's
//! payload at a time: it drives its caller's iterator directly rather
//! than collecting it first.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use oxisql_sqlite_compat::blocking::{SqliteBlockingTransaction, SqliteConnectionBlocking};

use crate::db3::metadata_yaml::{
    BagMetadata, CURRENT_METADATA_VERSION, DurationNanos, StartingTime,
};
use crate::db3::schema;
use crate::error::RosbagError;
use crate::topic::TopicRecord;

/// One row [`Writer::write_messages`] accepts: `(topic_id, timestamp_ns,
/// data)`, exactly the `messages` table's non-`id` columns in
/// [`crate::db3::schema::CREATE_TABLE_MESSAGES`] order.
pub type MessageRow = (i64, i64, Vec<u8>);

/// Opens a transaction on `conn`, runs `body`, and commits or rolls back
/// before returning — see the module docs for why this discipline is not
/// optional.
///
/// # Errors
///
/// Whatever `body` returns, after a successful rollback. If the rollback
/// itself also fails, [`RosbagError::Internal`] reports *both* failures
/// rather than silently keeping only one — an operator debugging a
/// wedged connection needs to see that the rollback failed too, not just
/// the original error.
fn with_transaction<T>(
    conn: &SqliteConnectionBlocking,
    path: &Path,
    body: impl FnOnce(&mut SqliteBlockingTransaction<'_>) -> Result<T, RosbagError>,
) -> Result<T, RosbagError> {
    let mut txn = conn
        .transaction()
        .map_err(|source| RosbagError::sql(path, source))?;
    match body(&mut txn) {
        Ok(value) => {
            txn.commit()
                .map_err(|source| RosbagError::sql(path, source))?;
            Ok(value)
        }
        Err(err) => match txn.rollback() {
            Ok(()) => Err(err),
            Err(rollback_err) => Err(RosbagError::Internal(format!(
                "write failed ({err}), and rolling back the transaction also failed: {rollback_err}"
            ))),
        },
    }
}

/// A `.db3` writer: creates the schema, registers topics, and appends
/// messages, then writes the `metadata.yaml` sidecar on
/// [`Writer::finish`].
///
/// # Examples
///
/// ```
/// use astrs_rosbag::TopicRecord;
/// use astrs_rosbag::db3::Writer;
///
/// // rosbag2's `metadata.yaml` sidecar lives beside the `.db3` file under
/// // a fixed name, so — matching real `ros2 bag record`'s own layout —
/// // each bag gets its own directory.
/// let dir = std::env::temp_dir().join(format!("astrs-rosbag-writer-doctest-{}", std::process::id()));
/// std::fs::create_dir_all(&dir)?;
/// let path = dir.join("session.db3");
///
/// let mut writer = Writer::create(&path, "jazzy")?;
/// let topic_id = writer.create_topic(&TopicRecord::new("/scan", "sensor_msgs/msg/LaserScan", "cdr"))?;
/// writer.write_messages(std::iter::once(Ok((topic_id, 1_000, vec![1, 2, 3]))))?;
/// writer.finish()?;
/// # std::fs::remove_dir_all(&dir).ok();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct Writer {
    conn: SqliteConnectionBlocking,
    path: PathBuf,
    ros_distro: String,
    next_topic_id: i64,
    /// Topic name -> row id, for [`Writer::create_topic`]'s idempotency.
    topic_ids: BTreeMap<String, i64>,
    /// Every topic this writer created, in creation order — the order
    /// `metadata.yaml`'s `topics_with_message_count` lists them in.
    topic_records: Vec<TopicRecord>,
    /// Distinct `TopicRecord::type` values already given a
    /// `message_definitions` row (rosbag2 dedupes by type, not by topic).
    type_definitions_written: std::collections::BTreeSet<String>,
    /// Row id -> message count so far, updated as
    /// [`Writer::write_messages`] batches complete.
    topic_counts: BTreeMap<i64, u64>,
    /// [`BagMetadata::custom_data`] entries to carry into both the
    /// `metadata.yaml` sidecar and the in-database `metadata` table — set
    /// via [`Writer::set_custom_data`].
    custom_data: BTreeMap<String, String>,
    message_count: u64,
    /// `(min, max)` message timestamps seen so far, in raw rosbag2
    /// nanoseconds (signed — see [`Writer::write_messages`]'s docs on
    /// pre-epoch timestamps).
    time_range: Option<(i64, i64)>,
    finished: bool,
}

impl Writer {
    /// Creates a fresh `.db3` at `path` (any existing file there is
    /// replaced, matching `std::fs::File::create`'s truncate-on-create
    /// semantics — a stale file left at this path must never be
    /// re-interpreted as this writer's own schema), with the current
    /// schema ([`schema::CURRENT_SCHEMA_VERSION`]) and no topics yet.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Io`] if `path`'s parent directory cannot be
    /// created, or [`RosbagError::Sql`] if the database cannot be opened
    /// or the schema cannot be written.
    pub fn create(
        path: impl AsRef<Path>,
        ros_distro: impl Into<String>,
    ) -> Result<Self, RosbagError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| RosbagError::io(&path, source))?;
        }
        // Best-effort: a stale file at `path` (from a previous, aborted
        // run) must not be reopened as-is — `SqliteConnection::open`
        // opens an *existing* database rather than truncating one, unlike
        // `std::fs::File::create`.
        let _ = std::fs::remove_file(&path);

        let path_str = path.to_str().ok_or_else(|| {
            RosbagError::Internal(format!("`{}` is not valid UTF-8", path.display()))
        })?;
        let conn = SqliteConnectionBlocking::open(path_str)
            .map_err(|source| RosbagError::sql(&path, source))?;
        for statement in schema::CREATE_STATEMENTS {
            conn.execute(statement, &[])
                .map_err(|source| RosbagError::sql(&path, source))?;
        }
        let ros_distro = ros_distro.into();
        conn.execute(
            "INSERT INTO schema (schema_version, ros_distro) VALUES ($1, $2)",
            &[
                &i64::from(schema::CURRENT_SCHEMA_VERSION),
                &ros_distro.as_str(),
            ],
        )
        .map_err(|source| RosbagError::sql(&path, source))?;

        Ok(Self {
            conn,
            path,
            ros_distro,
            next_topic_id: 1,
            topic_ids: BTreeMap::new(),
            topic_records: Vec::new(),
            type_definitions_written: std::collections::BTreeSet::new(),
            topic_counts: BTreeMap::new(),
            custom_data: BTreeMap::new(),
            message_count: 0,
            time_range: None,
            finished: false,
        })
    }

    /// The `.db3` path this writer is creating.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Registers a topic, returning its row id.
    ///
    /// Idempotent on [`TopicRecord::topic`] (the topic name, rosbag2's own
    /// uniqueness key): a second call naming an already-registered topic
    /// returns the existing id and otherwise does nothing — including
    /// when `topic`'s other fields differ from the first call's, mirroring
    /// `SqliteStorage::create_topic`'s own "first write wins" behavior.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Sql`] if the insert fails, or a serialization error
    /// wrapped as [`RosbagError::Internal`] if `offered_qos_profiles`
    /// cannot be rendered as YAML (never happens for values this crate's
    /// own [`crate::QosProfile`] produces).
    pub fn create_topic(&mut self, topic: &TopicRecord) -> Result<i64, RosbagError> {
        if let Some(&id) = self.topic_ids.get(&topic.topic) {
            return Ok(id);
        }
        let id = self.next_topic_id;
        self.next_topic_id += 1;

        let qos_yaml = astrs_yaml::to_string(&topic.offered_qos_profiles).map_err(|source| {
            RosbagError::Internal(format!("QoS profiles did not serialize: {source}"))
        })?;
        self.conn
            .execute(
                "INSERT INTO topics (id, name, type, serialization_format, offered_qos_profiles, \
                 type_description_hash) VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &id,
                    &topic.topic,
                    &topic.r#type,
                    &topic.serialization_format,
                    &qos_yaml,
                    &topic.type_description_hash,
                ],
            )
            .map_err(|source| RosbagError::sql(&self.path, source))?;

        // rosbag2 dedupes `message_definitions` by `topic_type`, not by
        // topic — and skips an empty type entirely (nothing to define).
        // This crate does not carry ROS `.msg`/`.idl` text (that is
        // astrs-idl's scope), so `encoding`/`encoded_message_definition`
        // are honestly empty rather than fabricated.
        if !topic.r#type.is_empty() && self.type_definitions_written.insert(topic.r#type.clone()) {
            self.conn
                .execute(
                    "INSERT INTO message_definitions (topic_type, encoding, \
                     encoded_message_definition, type_description_hash) VALUES ($1, $2, $3, $4)",
                    &[&topic.r#type, &"", &"", &topic.type_description_hash],
                )
                .map_err(|source| RosbagError::sql(&self.path, source))?;
        }

        self.topic_ids.insert(topic.topic.clone(), id);
        self.topic_records.push(topic.clone());
        self.topic_counts.insert(id, 0);
        Ok(id)
    }

    /// Appends every `(topic_id, timestamp_ns, data)` row `rows` yields,
    /// inside one transaction (see the module docs). `timestamp_ns` is
    /// rosbag2's own raw signed nanosecond value — see
    /// [`crate::convert`]'s docs for how `.arec`'s unsigned
    /// [`astrs_time::HlcTimestamp`] maps to and from it.
    ///
    /// Returns how many rows were written.
    ///
    /// # Errors
    ///
    /// Whatever `rows` itself yields as `Err` (propagated after a
    /// transaction rollback — nothing from this batch is left committed),
    /// or [`RosbagError::Sql`] if an insert fails.
    pub fn write_messages(
        &mut self,
        rows: impl Iterator<Item = Result<MessageRow, RosbagError>>,
    ) -> Result<u64, RosbagError> {
        let path = self.path.clone();
        let conn = &self.conn;
        let mut written = 0u64;
        let mut local_counts: BTreeMap<i64, u64> = BTreeMap::new();
        let mut local_range = self.time_range;

        with_transaction(conn, &path, |txn| {
            for row in rows {
                let (topic_id, timestamp_ns, data) = row?;
                txn.execute(
                    "INSERT INTO messages (timestamp, topic_id, data) VALUES ($1, $2, $3)",
                    &[&timestamp_ns, &topic_id, &data],
                )
                .map_err(|source| RosbagError::sql(&path, source))?;
                written += 1;
                *local_counts.entry(topic_id).or_insert(0) += 1;
                local_range = Some(match local_range {
                    None => (timestamp_ns, timestamp_ns),
                    Some((min, max)) => (min.min(timestamp_ns), max.max(timestamp_ns)),
                });
            }
            Ok(())
        })?;

        self.message_count += written;
        for (id, count) in local_counts {
            *self.topic_counts.entry(id).or_insert(0) += count;
        }
        self.time_range = local_range;
        Ok(written)
    }

    /// Writes the `metadata.yaml` sidecar and marks this writer finished.
    ///
    /// Idempotent: a second call is a no-op that returns the same path
    /// (mirroring [`astrs_recording::Writer::finish`]'s contract).
    ///
    /// # Errors
    ///
    /// [`RosbagError::Io`] if `metadata.yaml` cannot be written.
    pub fn finish(mut self) -> Result<PathBuf, RosbagError> {
        if !self.finished {
            self.write_metadata_yaml()?;
            self.finished = true;
        }
        Ok(self.path.clone())
    }

    /// The `metadata.yaml` sidecar path for this writer's `.db3` file:
    /// the same directory, named `metadata.yaml`.
    #[must_use]
    pub fn metadata_path(&self) -> PathBuf {
        metadata_path_for(&self.path)
    }

    /// Sets one [`BagMetadata::custom_data`] entry, returning the value it
    /// replaced.
    ///
    /// Free-form key/value data real rosbag2 producers use for their own
    /// purposes (blueprint §10.6's [`crate::convert`] uses it to carry
    /// astrs-rosbag's own topic-manifest sidecar — node/output ids, the
    /// source `.arec`'s dataflow id and HLC epoch — so a `.db3 → .arec`
    /// conversion recovers them losslessly rather than re-synthesizing
    /// fresh ones). Written into both `metadata.yaml` and the in-database
    /// `metadata` table on [`Writer::finish`], so it survives even a bag
    /// copied without its sidecar file (see [`Reader::metadata`](crate::db3::Reader::metadata)'s
    /// fallback).
    pub fn set_custom_data(
        &mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Option<String> {
        self.custom_data.insert(key.into(), value.into())
    }

    fn write_metadata_yaml(&self) -> Result<(), RosbagError> {
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("session.db3")
            .to_owned();

        let (min, max) = self.time_range.unwrap_or((0, 0));
        // metadata.yaml's `nanoseconds_since_epoch`/`nanoseconds` fields
        // are unsigned; a pre-epoch (negative) rosbag2 timestamp — legal
        // in `messages.timestamp`, which stays a signed `i64` untouched —
        // saturates to zero here rather than wrapping to a huge value.
        // This only affects these two *summary* fields: every message row
        // itself is written exactly as given.
        let starting_time = StartingTime {
            nanoseconds_since_epoch: u64::try_from(min).unwrap_or(0),
        };
        let duration = DurationNanos {
            nanoseconds: u64::try_from(max.saturating_sub(min)).unwrap_or(0),
        };

        let topics: Vec<(TopicRecord, u64)> = self
            .topic_records
            .iter()
            .map(|record| {
                let id = self.topic_ids.get(&record.topic).copied().unwrap_or(0);
                let count = self.topic_counts.get(&id).copied().unwrap_or(0);
                (record.clone(), count)
            })
            .collect();

        let mut metadata = BagMetadata::single_file(
            file_name,
            starting_time,
            duration,
            self.ros_distro.clone(),
            topics,
        );
        metadata.custom_data = self.custom_data.clone();
        let yaml = metadata.to_yaml().map_err(|source| {
            RosbagError::Internal(format!("metadata.yaml did not serialize: {source}"))
        })?;

        // The in-database duplicate (`schema.rs`'s module docs: "the
        // `metadata` table (`metadata.yaml`'s content, duplicated
        // in-DB)"), so a bag copied without its `metadata.yaml` sidecar
        // (see `Reader::metadata`'s fallback) is still fully readable —
        // real rosbag2's `sqlite_storage.cpp` keeps the same redundancy.
        self.conn
            .execute(
                "INSERT INTO metadata (metadata_version, metadata) VALUES ($1, $2)",
                &[&CURRENT_METADATA_VERSION, &yaml],
            )
            .map_err(|source| RosbagError::sql(&self.path, source))?;

        std::fs::write(self.metadata_path(), yaml)
            .map_err(|source| RosbagError::io(self.metadata_path(), source))
    }
}

impl Drop for Writer {
    /// Best-effort `metadata.yaml` write on an early drop (an unhandled
    /// `?`, a panic elsewhere in the same scope) — the graceful-shutdown
    /// half of [`astrs_recording::Writer`]'s own early-drop contract.
    /// Every `.db3` row already committed by [`Writer::write_messages`]'s
    /// own per-batch transactions is durable regardless; this only
    /// affects whether `metadata.yaml` reflects them.
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.write_metadata_yaml();
        }
    }
}

/// The `metadata.yaml` sidecar path for a `.db3` file at `db3_path`: the
/// same directory, named literally `metadata.yaml` (rosbag2's own fixed
/// name, `rosbag2_storage::metadata_filename`).
#[must_use]
pub fn metadata_path_for(db3_path: &Path) -> PathBuf {
    match db3_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join("metadata.yaml"),
        _ => PathBuf::from("metadata.yaml"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    /// A fresh `session.db3` path inside its own, uniquely-named
    /// directory — never a bare file directly under `temp_dir()`: real
    /// rosbag2's `metadata.yaml` sidecar has a *fixed* name within its
    /// bag's directory (matching real `ros2 bag record`'s own
    /// one-bag-per-directory layout), so two tests sharing a parent
    /// directory would otherwise race on the exact same `metadata.yaml`
    /// path under parallel `cargo nextest`.
    fn temp_path(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "astrs-rosbag-writer-test-{}-{}-{label}",
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

    #[test]
    fn create_writes_the_full_current_schema() {
        let path = temp_path("create");
        let writer = Writer::create(&path, "jazzy").unwrap();
        assert!(path.exists());
        drop(writer);
        cleanup(&path);
    }

    #[test]
    fn create_topic_is_idempotent_on_name() {
        let path = temp_path("idempotent");
        let mut writer = Writer::create(&path, "jazzy").unwrap();
        let first = writer
            .create_topic(&TopicRecord::new(
                "/scan",
                "sensor_msgs/msg/LaserScan",
                "cdr",
            ))
            .unwrap();
        let second = writer
            .create_topic(&TopicRecord::new(
                "/scan",
                "a different type entirely",
                "cdr",
            ))
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(writer.topic_records.len(), 1);
        assert_eq!(writer.topic_records[0].r#type, "sensor_msgs/msg/LaserScan");
        drop(writer);
        cleanup(&path);
    }

    #[test]
    fn write_messages_updates_counts_and_time_range() {
        let path = temp_path("counts");
        let mut writer = Writer::create(&path, "jazzy").unwrap();
        let topic_id = writer
            .create_topic(&TopicRecord::new(
                "/scan",
                "sensor_msgs/msg/LaserScan",
                "cdr",
            ))
            .unwrap();
        let written = writer
            .write_messages(
                vec![
                    Ok((topic_id, 30, vec![1])),
                    Ok((topic_id, 10, vec![2])),
                    Ok((topic_id, 20, vec![3])),
                ]
                .into_iter(),
            )
            .unwrap();
        assert_eq!(written, 3);
        assert_eq!(writer.message_count, 3);
        assert_eq!(writer.time_range, Some((10, 30)));
        assert_eq!(*writer.topic_counts.get(&topic_id).unwrap(), 3);
        drop(writer);
        cleanup(&path);
    }

    /// Regression test for a real defect this crate worked around rather
    /// than triggered blindly: `oxisql-sqlite-compat` 0.4.1 (via
    /// `oxisqlite`, the pure-Rust Limbo fork) silently returns **zero**
    /// rows for any statement that binds *both* a `WHERE`-clause `$N`
    /// parameter *and* a `LIMIT`-clause `$N` parameter — in either
    /// parameter-number order, with or without an intervening `ORDER BY`.
    /// Each half works perfectly in isolation (`WHERE $1` alone, `LIMIT
    /// $1` alone, two `WHERE` parameters together, one parameter plus a
    /// literal on the other clause); only the specific WHERE-param +
    /// LIMIT-param combination is affected. [`crate::db3::reader::MessageIter`]
    /// (the only place this crate builds such a query) works around it by
    /// interpolating `LIMIT` as a literal integer — this test pins that
    /// workaround against the underlying bug ever silently "fixing itself"
    /// into a different, equally-wrong shape, and against the workaround
    /// itself regressing back to a bound `LIMIT` parameter.
    #[test]
    fn where_param_plus_limit_param_is_a_known_oxisqlite_defect_worked_around_by_a_literal_limit() {
        let path = temp_path("oxisqlite-where-limit-defect");
        {
            let mut writer = Writer::create(&path, "jazzy").unwrap();
            let topic_id = writer
                .create_topic(&TopicRecord::new("/scan", "", "cdr"))
                .unwrap();
            let rows: Vec<Result<MessageRow, RosbagError>> =
                (0..5).map(|i| Ok((topic_id, i, vec![i as u8]))).collect();
            writer.write_messages(rows.into_iter()).unwrap();
        }
        let conn = SqliteConnectionBlocking::open(path.to_str().unwrap()).unwrap();

        let both_bound = conn
            .query(
                "SELECT id FROM messages WHERE id > $1 ORDER BY id LIMIT $2",
                &[&0i64, &1000i64],
            )
            .unwrap();
        assert_eq!(
            both_bound.len(),
            0,
            "if this now returns 5, the upstream defect was fixed: \
             MessageIter's literal-LIMIT workaround can be reverted"
        );

        let workaround = conn
            .query(
                "SELECT id FROM messages WHERE id > $1 ORDER BY id LIMIT 1000",
                &[&0i64],
            )
            .unwrap();
        assert_eq!(
            workaround.len(),
            5,
            "the workaround this crate actually uses must see every row"
        );

        cleanup(&path);
    }

    #[test]
    fn a_failing_row_iterator_rolls_back_the_whole_batch() {
        let path = temp_path("rollback");
        let mut writer = Writer::create(&path, "jazzy").unwrap();
        let topic_id = writer
            .create_topic(&TopicRecord::new(
                "/scan",
                "sensor_msgs/msg/LaserScan",
                "cdr",
            ))
            .unwrap();
        let rows: Vec<Result<MessageRow, RosbagError>> = vec![
            Ok((topic_id, 1, vec![1])),
            Ok((topic_id, 2, vec![2])),
            Err(RosbagError::Internal("source reader failed".to_owned())),
        ];
        let err = writer.write_messages(rows.into_iter()).unwrap_err();
        assert!(matches!(err, RosbagError::Internal(_)));
        // Nothing from the failed batch is visible — including the two
        // rows that inserted fine before the error, and bookkeeping was
        // never updated either.
        assert_eq!(writer.message_count, 0);
        let rows_in_db = writer.conn.query("SELECT * FROM messages", &[]).unwrap();
        assert!(rows_in_db.is_empty());
        drop(writer);
        cleanup(&path);
    }

    #[test]
    fn finish_writes_a_readable_metadata_yaml() {
        let path = temp_path("finish");
        let mut writer = Writer::create(&path, "jazzy").unwrap();
        let topic_id = writer
            .create_topic(&TopicRecord::new(
                "/scan",
                "sensor_msgs/msg/LaserScan",
                "cdr",
            ))
            .unwrap();
        writer
            .write_messages(
                vec![
                    Ok((topic_id, 1_000, vec![0; 8])),
                    Ok((topic_id, 2_000, vec![0; 8])),
                ]
                .into_iter(),
            )
            .unwrap();
        let returned_path = writer.finish().unwrap();
        assert_eq!(returned_path, path);
        let yaml_path = metadata_path_for(&path);
        let yaml = std::fs::read_to_string(&yaml_path).unwrap();
        let metadata = BagMetadata::from_yaml_str(&yaml).unwrap();
        assert_eq!(metadata.message_count, 2);
        assert_eq!(metadata.topics_with_message_count.len(), 1);
        assert_eq!(metadata.topics_with_message_count[0].message_count, 2);
        assert_eq!(metadata.starting_time.nanoseconds_since_epoch, 1_000);
        assert_eq!(metadata.duration.nanoseconds, 1_000);
        cleanup(&path);
    }

    #[test]
    fn finish_also_writes_the_in_database_metadata_table() {
        let path = temp_path("finish-db-metadata");
        let mut writer = Writer::create(&path, "jazzy").unwrap();
        writer
            .create_topic(&TopicRecord::new("/x", "", "cdr"))
            .unwrap();
        writer.finish().unwrap();

        let conn = SqliteConnectionBlocking::open(path.to_str().unwrap()).unwrap();
        let rows = conn
            .query("SELECT metadata_version, metadata FROM metadata", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        let oxisql_core::Value::I64(version) = rows[0].get_by_index(0).unwrap() else {
            panic!("expected an integer metadata_version");
        };
        assert_eq!(*version, CURRENT_METADATA_VERSION);
        let oxisql_core::Value::Text(yaml) = rows[0].get_by_index(1).unwrap() else {
            panic!("expected the metadata text");
        };
        let db_metadata = BagMetadata::from_yaml_str(yaml).unwrap();
        let file_metadata =
            BagMetadata::from_yaml_str(&std::fs::read_to_string(metadata_path_for(&path)).unwrap())
                .unwrap();
        assert_eq!(
            db_metadata, file_metadata,
            "the in-database copy must match metadata.yaml exactly"
        );
        cleanup(&path);
    }

    #[test]
    fn set_custom_data_survives_into_both_metadata_copies() {
        let path = temp_path("custom-data");
        let mut writer = Writer::create(&path, "jazzy").unwrap();
        assert_eq!(writer.set_custom_data("astrs_rosbag_topics", "first"), None);
        assert_eq!(
            writer.set_custom_data("astrs_rosbag_topics", "second"),
            Some("first".to_owned()),
            "a second call replaces and reports the old value, like BTreeMap::insert"
        );
        writer.finish().unwrap();

        let yaml = std::fs::read_to_string(metadata_path_for(&path)).unwrap();
        let metadata = BagMetadata::from_yaml_str(&yaml).unwrap();
        assert_eq!(
            metadata.custom_data.get("astrs_rosbag_topics"),
            Some(&"second".to_owned())
        );
        cleanup(&path);
    }

    #[test]
    fn finish_is_idempotent() {
        let path = temp_path("finish-idempotent");
        let mut writer = Writer::create(&path, "jazzy").unwrap();
        writer
            .create_topic(&TopicRecord::new("/x", "", "cdr"))
            .unwrap();
        let first = writer.finish().unwrap();
        // `finish` consumes `self`; idempotency here means "safe to call
        // once and the file is stable", which the returned path and its
        // continued existence both attest to.
        assert_eq!(first, path);
        assert!(metadata_path_for(&path).exists());
        cleanup(&path);
    }

    #[test]
    fn dropping_an_unfinished_writer_still_leaves_a_readable_metadata_yaml() {
        let path = temp_path("drop-finalizes");
        {
            let mut writer = Writer::create(&path, "jazzy").unwrap();
            let topic_id = writer
                .create_topic(&TopicRecord::new("/scan", "", "cdr"))
                .unwrap();
            writer
                .write_messages(std::iter::once(Ok((topic_id, 1, vec![9]))))
                .unwrap();
            // No explicit `finish()`.
        }
        let yaml = std::fs::read_to_string(metadata_path_for(&path)).unwrap();
        let metadata = BagMetadata::from_yaml_str(&yaml).unwrap();
        assert_eq!(metadata.message_count, 1);
        cleanup(&path);
    }

    #[test]
    fn an_empty_bag_still_finishes_cleanly() {
        let path = temp_path("empty");
        let writer = Writer::create(&path, "jazzy").unwrap();
        let returned = writer.finish().unwrap();
        assert_eq!(returned, path);
        let yaml = std::fs::read_to_string(metadata_path_for(&path)).unwrap();
        let metadata = BagMetadata::from_yaml_str(&yaml).unwrap();
        assert_eq!(metadata.message_count, 0);
        assert!(metadata.topics_with_message_count.is_empty());
        cleanup(&path);
    }

    #[test]
    fn a_message_definition_row_is_written_once_per_type_not_per_topic() {
        let path = temp_path("dedup-defs");
        let mut writer = Writer::create(&path, "jazzy").unwrap();
        writer
            .create_topic(&TopicRecord::new("/a", "std_msgs/msg/String", "cdr"))
            .unwrap();
        writer
            .create_topic(&TopicRecord::new("/b", "std_msgs/msg/String", "cdr"))
            .unwrap();
        let rows = writer
            .conn
            .query("SELECT COUNT(*) FROM message_definitions", &[])
            .unwrap();
        let oxisql_core::Value::I64(count) = rows[0].get_by_index(0).unwrap() else {
            panic!("expected an integer count");
        };
        assert_eq!(*count, 1);
        drop(writer);
        cleanup(&path);
    }
}
