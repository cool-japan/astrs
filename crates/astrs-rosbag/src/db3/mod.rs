//! rosbag2 `.db3` — read and write, without libsqlite3 (blueprint §10.6).
//!
//! rosbag2 stores a bag as one or more SQLite databases plus a
//! `metadata.yaml` sidecar next to them ([`metadata_yaml`]). This module
//! speaks the SQLite half through `oxisql-sqlite-compat` (never
//! `rusqlite`/`libsqlite3-sys` — see astrs.md §18): [`schema`] is the DDL
//! rosbag2's own `sqlite_storage.cpp` writes, [`Reader`] opens an existing
//! `.db3` (tolerating every schema version rosbag2 has shipped), and
//! [`Writer`] creates a fresh one at the current schema.
//!
//! # Schema version
//!
//! [`Writer`] always writes schema version [`schema::CURRENT_SCHEMA_VERSION`]
//! (rosbag2's `kDBSchemaVersion_ = 4`, the `message_definitions` table
//! plus `topics.type_description_hash`) and `metadata.yaml` version
//! [`metadata_yaml::CURRENT_METADATA_VERSION`] (`9`, structured
//! `offered_qos_profiles`). **This targets Jazzy/Rolling-era ROS 2**:
//! Humble's `rosbag2_storage_sqlite3` plugin does not understand schema
//! version 4 and will refuse to open a bag this crate wrote. [`Reader`]
//! reads every version back to 1.
//!
//! # `metadata.yaml`'s in-database twin
//!
//! Schema version 3 onward also carries the `metadata` table: `metadata.yaml`'s
//! own content, duplicated inside the database (real rosbag2's own
//! redundancy). [`Writer::finish`] always writes both copies, and
//! [`Reader::open`] falls back to the in-database one when the external
//! file is missing or will not parse — so a bag copied without its
//! sidecar file is still fully readable. [`Writer::set_custom_data`] is
//! how [`crate::convert::arec_to_db3::arec_to_db3`] rides its own
//! topic-manifest sidecar through this same redundancy, into
//! [`BagMetadata::custom_data`].

pub mod metadata_yaml;
pub mod reader;
pub mod schema;
pub mod writer;

pub use metadata_yaml::BagMetadata;
pub use reader::Reader;
pub use writer::{MessageRow, Writer};
