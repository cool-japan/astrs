//! rosbag2 files, without libsqlite3.
//!
//! Existing robot data opens natively in AstRS (blueprint §10.6):
//!
//! - `.db3` reading and writing through `oxisql-sqlite-compat`, so there is
//!   no C SQLite in the build.
//! - `.mcap` reading, including the chunked and indexed layouts.
//! - Conversion in both directions between rosbag2 files and the AstRS
//!   `.arec` container, backing `astrs bag convert` and `astrs bag info`.

pub mod convert;
pub mod db3;
pub mod error;
pub mod mcap;
pub mod topic;

pub use convert::{BagFormat, ConversionReport, convert_bag, detect_format};
pub use error::RosbagError;
pub use topic::{
    OUTPUT_NAME, QosDuration, QosProfile, RAW_SERIALIZATION_FORMAT, RosbagTopicManifest,
    SIDECAR_FORMAT_VERSION, SIDECAR_KEY, SidecarTopic, TopicRecord, assign_node_ids,
};
