//! The value types stored in each bucket and logged in the mutation log.
//!
//! See the crate root for the JSON-vs-oxicode envelope decision that shapes
//! every type here: each record derives both `serde::{Serialize,
//! Deserialize}` (the bucket's on-disk JSON form) and `oxicode::{Encode,
//! Decode}` (the mutation log's on-disk form, reached through
//! [`astrs_wire::WireEncode`] / [`astrs_wire::WireDecode`]), so the exact
//! same Rust value can be written to either.

mod build_cache;
mod daemon;
mod dataflow;
mod mutation;
mod param;

pub use build_cache::{BuildCacheEntry, BuildCacheKey};
pub use daemon::DaemonRecord;
pub use dataflow::{DataflowMeta, DataflowSnapshot, NodeStatusRecord};
pub use mutation::{CatchUpBatch, MutationOp, MutationRecord, MutationSeq};
pub use param::ParamRecord;
