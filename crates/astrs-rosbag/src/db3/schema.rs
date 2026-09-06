//! The rosbag2 `.db3` SQLite schema, as `rosbag2_storage_sqlite3`'s
//! `SqliteStorage::initialize()` writes it (`sqlite_storage.cpp`,
//! `kDBSchemaVersion_ = 4`) — reproduced here as DDL constants rather than
//! guessed, since a `.db3` this crate writes has to open in real ROS 2
//! tooling.
//!
//! # Version history (what [`Reader`](crate::db3::Reader) tolerates)
//!
//! | Version | Added |
//! |---|---|
//! | 1 | `topics(id, name, type, serialization_format)`, `messages` |
//! | 2 | `topics.offered_qos_profiles` |
//! | 3 | the `schema` table (this DB's own schema version + `ros_distro`); the `metadata` table (`metadata.yaml`'s content, duplicated in-DB) |
//! | 4 | `topics.type_description_hash`; the `message_definitions` table |
//!
//! [`Writer`](crate::db3::Writer) always writes version
//! [`CURRENT_SCHEMA_VERSION`] (4) — see the module docs for the Humble
//! incompatibility this implies. [`detect_schema_version`] and the
//! column-presence helpers let [`Reader`](crate::db3::Reader) open any of
//! the four without guessing from the version number alone, mirroring
//! `fill_topics_and_types()`'s own column-presence checks rather than
//! trusting `schema_version` blindly for a decision the database itself
//! can answer directly.

use crate::error::RosbagError;

/// The schema version [`Writer`](crate::db3::Writer) writes and
/// [`detect_schema_version`] reports for a fresh database: `topics` has
/// both `offered_qos_profiles` and `type_description_hash`, and
/// `message_definitions` exists.
pub const CURRENT_SCHEMA_VERSION: u32 = 4;

/// `CREATE TABLE schema(...)` — this database's own schema version and
/// the ROS distro that wrote it. Present from version 3 onward.
pub const CREATE_TABLE_SCHEMA: &str =
    "CREATE TABLE schema(schema_version INTEGER PRIMARY KEY, ros_distro TEXT NOT NULL)";

/// `CREATE TABLE metadata(...)` — `metadata.yaml`'s content, duplicated
/// in-database. Present from version 3 onward.
pub const CREATE_TABLE_METADATA: &str = "CREATE TABLE metadata(id INTEGER PRIMARY KEY, metadata_version INTEGER NOT NULL, metadata TEXT NOT NULL)";

/// `CREATE TABLE topics(...)` at the current (version 4) column set.
pub const CREATE_TABLE_TOPICS: &str = "CREATE TABLE topics(\
    id INTEGER PRIMARY KEY, \
    name TEXT NOT NULL, \
    type TEXT NOT NULL, \
    serialization_format TEXT NOT NULL, \
    offered_qos_profiles TEXT NOT NULL, \
    type_description_hash TEXT NOT NULL)";

/// `CREATE TABLE message_definitions(...)`. Present from version 4 onward.
pub const CREATE_TABLE_MESSAGE_DEFINITIONS: &str = "CREATE TABLE message_definitions(\
    id INTEGER PRIMARY KEY, \
    topic_type TEXT NOT NULL, \
    encoding TEXT NOT NULL, \
    encoded_message_definition TEXT NOT NULL, \
    type_description_hash TEXT NOT NULL)";

/// `CREATE TABLE messages(...)` — unchanged across every schema version.
pub const CREATE_TABLE_MESSAGES: &str = "CREATE TABLE messages(\
    id INTEGER PRIMARY KEY, \
    topic_id INTEGER NOT NULL, \
    timestamp INTEGER NOT NULL, \
    data BLOB NOT NULL)";

/// `CREATE INDEX timestamp_idx ON messages (timestamp ASC)`.
pub const CREATE_INDEX_TIMESTAMP: &str = "CREATE INDEX timestamp_idx ON messages (timestamp ASC)";

/// Every `CREATE TABLE`/`CREATE INDEX` statement [`Writer`](crate::db3::Writer)
/// issues, in the order `SqliteStorage::initialize()` issues them.
pub const CREATE_STATEMENTS: [&str; 6] = [
    CREATE_TABLE_SCHEMA,
    CREATE_TABLE_METADATA,
    CREATE_TABLE_TOPICS,
    CREATE_TABLE_MESSAGE_DEFINITIONS,
    CREATE_TABLE_MESSAGES,
    CREATE_INDEX_TIMESTAMP,
];

/// Whether `table` exists in the database `conn` is connected to.
///
/// # Errors
///
/// [`RosbagError::Sql`] if the introspection query itself fails.
pub fn table_exists(
    conn: &oxisql_sqlite_compat::blocking::SqliteConnectionBlocking,
    path: &std::path::Path,
    table: &str,
) -> Result<bool, RosbagError> {
    let tables = conn
        .tables()
        .map_err(|source| RosbagError::sql(path, source))?;
    Ok(tables.iter().any(|info| info.name == table))
}

/// Whether `table` has a column named `column`.
///
/// # Errors
///
/// [`RosbagError::Sql`] if the introspection query itself fails (including
/// when `table` does not exist, which `PRAGMA table_info` on Limbo answers
/// with zero rows rather than an error — so this returns `Ok(false)`, not
/// an error, for a missing table).
pub fn column_exists(
    conn: &oxisql_sqlite_compat::blocking::SqliteConnectionBlocking,
    path: &std::path::Path,
    table: &str,
    column: &str,
) -> Result<bool, RosbagError> {
    let columns = conn
        .columns(table)
        .map_err(|source| RosbagError::sql(path, source))?;
    Ok(columns.iter().any(|info| info.name == column))
}

/// Detects a `.db3`'s schema version, mirroring
/// `SqliteStorage::read_db_schema_version()` exactly: prefer the `schema`
/// table's own `schema_version` row when present, otherwise infer version
/// 1 or 2 from whether `topics.offered_qos_profiles` exists (the only two
/// versions old enough to predate the `schema` table).
///
/// # Errors
///
/// [`RosbagError::Sql`] if introspection fails, or
/// [`RosbagError::MissingTable`] when `topics` itself is absent — a file
/// that is not a rosbag2 database at all.
pub fn detect_schema_version(
    conn: &oxisql_sqlite_compat::blocking::SqliteConnectionBlocking,
    path: &std::path::Path,
) -> Result<u32, RosbagError> {
    if table_exists(conn, path, "schema")? {
        let rows = conn
            .query("SELECT schema_version FROM schema", &[])
            .map_err(|source| RosbagError::sql(path, source))?;
        if let Some(row) = rows.first()
            && let Some(oxisql_core::Value::I64(version)) = row.get_by_index(0)
        {
            return Ok(u32::try_from(*version).unwrap_or(0));
        }
        // A `schema` table with no row (or a row of the wrong type) is
        // corrupt, not merely old — fall through to the same "not a
        // rosbag2 database" reporting an entirely absent `topics` table
        // gets, since there is nothing honest left to infer.
    }
    if !table_exists(conn, path, "topics")? {
        return Err(RosbagError::MissingTable {
            path: path.to_path_buf(),
            table: "topics",
        });
    }
    if column_exists(conn, path, "topics", "offered_qos_profiles")? {
        Ok(2)
    } else {
        Ok(1)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use oxisql_sqlite_compat::blocking::SqliteConnectionBlocking;

    fn memory_conn() -> SqliteConnectionBlocking {
        SqliteConnectionBlocking::open_memory().unwrap()
    }

    #[test]
    fn create_statements_build_the_full_current_schema() {
        let conn = memory_conn();
        for stmt in CREATE_STATEMENTS {
            conn.execute(stmt, &[]).unwrap();
        }
        let path = std::path::Path::new(":memory:");
        assert!(table_exists(&conn, path, "schema").unwrap());
        assert!(table_exists(&conn, path, "metadata").unwrap());
        assert!(table_exists(&conn, path, "topics").unwrap());
        assert!(table_exists(&conn, path, "message_definitions").unwrap());
        assert!(table_exists(&conn, path, "messages").unwrap());
        assert!(!table_exists(&conn, path, "no_such_table").unwrap());
    }

    #[test]
    fn column_exists_is_false_for_a_missing_table() {
        let conn = memory_conn();
        let path = std::path::Path::new(":memory:");
        assert!(!column_exists(&conn, path, "topics", "name").unwrap());
    }

    #[test]
    fn detect_schema_version_reads_the_schema_table_when_present() {
        let conn = memory_conn();
        conn.execute(CREATE_TABLE_SCHEMA, &[]).unwrap();
        conn.execute(
            "INSERT INTO schema (schema_version, ros_distro) VALUES ($1, $2)",
            &[&4i64, &"jazzy"],
        )
        .unwrap();
        let path = std::path::Path::new(":memory:");
        assert_eq!(detect_schema_version(&conn, path).unwrap(), 4);
    }

    #[test]
    fn detect_schema_version_infers_two_from_the_qos_column() {
        let conn = memory_conn();
        conn.execute(
            "CREATE TABLE topics(id INTEGER PRIMARY KEY, name TEXT NOT NULL, type TEXT NOT NULL, \
             serialization_format TEXT NOT NULL, offered_qos_profiles TEXT NOT NULL)",
            &[],
        )
        .unwrap();
        let path = std::path::Path::new(":memory:");
        assert_eq!(detect_schema_version(&conn, path).unwrap(), 2);
    }

    #[test]
    fn detect_schema_version_infers_one_without_the_qos_column() {
        let conn = memory_conn();
        conn.execute(
            "CREATE TABLE topics(id INTEGER PRIMARY KEY, name TEXT NOT NULL, type TEXT NOT NULL, \
             serialization_format TEXT NOT NULL)",
            &[],
        )
        .unwrap();
        let path = std::path::Path::new(":memory:");
        assert_eq!(detect_schema_version(&conn, path).unwrap(), 1);
    }

    #[test]
    fn detect_schema_version_rejects_a_database_with_no_topics_table_at_all() {
        let conn = memory_conn();
        let path = std::path::Path::new("not-a-bag.db3");
        let err = detect_schema_version(&conn, path).unwrap_err();
        assert!(matches!(
            err,
            RosbagError::MissingTable {
                table: "topics",
                ..
            }
        ));
    }
}
