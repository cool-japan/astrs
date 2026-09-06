//! The `.arec` recording container.
//!
//! Determinism as a feature: an execution is a file you can replay
//! (blueprint §14):
//!
//! - Container format v1, magic `ASTRSREC`: a header carrying the HLC
//!   epoch, the dataflow id and the embedded manifest ([`Header`]);
//!   zstd-framed (oxiarc) entry blocks of `{node, output, hlc, meta,
//!   payload}` ([`Entry`]); a seekable index footer ([`index::Footer`]).
//! - [`Writer`] streams entries with bounded memory and crash-tolerant
//!   durability; [`Reader`] opens a finished file by its trailer, or
//!   (via [`recover::scan`]) a truncated one by scanning; [`merge::merge`]
//!   k-way merges several recordings by HLC into one.
//! - [`stats::Stats`] answers `astrs bag info`-shaped questions from the
//!   footer index alone, without touching a single payload.
//! - The shared vocabulary used by `astrs-record-node`, `astrs-replay-node`,
//!   daemon-side `astrs record start`, and (eventually) `.arec ⇄ rosbag2`
//!   conversion.
//!
//! # File layout
//!
//! See [`mod@format`] for the exact byte-level shape every header, entry
//! and footer frame shares, the fixed trailer that makes the footer
//! seekable in one read, and the recovery contract a truncated file
//! gets.
//!
//! # Example: write, then read back
//!
//! ```
//! use astrs_recording::{Entry, Reader, Writer, WriterOptions};
//! use astrs_time::HlcTimestamp;
//! use astrs_wire::{DataId, DataflowId, Metadata, NodeId};
//!
//! let path = std::env::temp_dir().join(format!("astrs-recording-lib-doctest-{}.arec", std::process::id()));
//!
//! let mut writer = Writer::create(
//!     &path,
//!     WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::new(1_000, 0))
//!         .with_manifest_yaml("nodes:\n  - id: camera\n    path: ./camera\n"),
//! )?;
//! writer.append(Entry::new(
//!     NodeId::new("camera")?,
//!     DataId::new("frames")?,
//!     Metadata::new(HlcTimestamp::new(1_000, 1)),
//!     vec![1, 2, 3],
//! ))?;
//! writer.finish()?;
//!
//! let mut reader = Reader::open(&path)?;
//! let entries: Vec<Entry> = reader.iter_all().collect::<Result<_, _>>()?;
//! assert_eq!(entries.len(), 1);
//! assert_eq!(entries[0].payload, vec![1, 2, 3]);
//! # std::fs::remove_file(&path).ok();
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod entry;
pub mod error;
pub mod format;
pub mod header;
pub mod index;
pub mod merge;
pub mod reader;
pub mod recover;
pub mod stats;
pub mod writer;

pub use entry::Entry;
pub use error::RecordingError;
pub use header::Header;
pub use index::{Footer, IndexEntry};
pub use merge::merge;
pub use reader::{EntryIter, Reader};
pub use recover::{RecoveryReport, StopReason, scan};
pub use stats::{PortStats, Stats};
pub use writer::{RotationPolicy, Writer, WriterOptions};
