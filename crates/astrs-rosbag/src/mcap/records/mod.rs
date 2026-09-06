//! MCAP record types — every field, byte width, and length-prefix shape
//! reproduced from the official specification (`github.com/foxglove/mcap`,
//! `website/docs/spec/{index.md,mcap.ksy}`, fetched and cross-checked
//! against each other; astrs.md §18 forbids C++-derived fixtures in-repo,
//! so this crate has no compiled `mcap` reference implementation to check
//! against either — the two independent spec artifacts are the ground
//! truth this module is built from).
//!
//! Split into [`data`] (the records that appear in an *unindexed* mcap:
//! `Header`, `Footer`, `Schema`, `Channel`, `Message`, `Chunk`, `DataEnd`)
//! and [`index`] (the summary-section-only records an *indexed* mcap adds:
//! `MessageIndex`, `ChunkIndex`, `Attachment`, `AttachmentIndex`,
//! `Statistics`, `Metadata`, `MetadataIndex`, `SummaryOffset`) purely to
//! keep each file well under the workspace's 2,000-line ceiling — the
//! split carries no other meaning, and [`crate::mcap::reader`] treats
//! every opcode uniformly.

pub mod data;
pub mod index;

pub use data::{Channel, Chunk, DataEnd, Footer, Header, Message, Schema};
pub use index::{
    Attachment, AttachmentIndex, ChunkIndex, MessageIndex, MessageIndexEntry, Metadata,
    MetadataIndex, Statistics, SummaryOffset,
};

/// The single-byte record type identifier every MCAP record opens with
/// (spec "Records" section). `0x00` is not a valid opcode; `0x10`-`0x7F`
/// are reserved for future MCAP use; `0x80`-`0xFF` are reserved for
/// private, application-specific records — [`Opcode::from_u8`] returns
/// `None` for all three, and [`crate::mcap::reader`] skips a record
/// bearing an unrecognized opcode rather than rejecting the file (the
/// spec's own forward-compatibility stance: "Readers should ignore any
/// unknown fields").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Opcode {
    /// `0x01`.
    Header,
    /// `0x02`.
    Footer,
    /// `0x03`.
    Schema,
    /// `0x04`.
    Channel,
    /// `0x05`.
    Message,
    /// `0x06`.
    Chunk,
    /// `0x07`.
    MessageIndex,
    /// `0x08`.
    ChunkIndex,
    /// `0x09`.
    Attachment,
    /// `0x0A`.
    AttachmentIndex,
    /// `0x0B`.
    Statistics,
    /// `0x0C`.
    Metadata,
    /// `0x0D`.
    MetadataIndex,
    /// `0x0E`.
    SummaryOffset,
    /// `0x0F`.
    DataEnd,
}

impl Opcode {
    /// Parses a record's leading opcode byte.
    ///
    /// Returns `None` for `0x00` and every reserved/private value —
    /// deliberately not an error, per the spec's own forward-compatibility
    /// stance (see the type's own docs).
    #[must_use]
    pub const fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0x01 => Some(Self::Header),
            0x02 => Some(Self::Footer),
            0x03 => Some(Self::Schema),
            0x04 => Some(Self::Channel),
            0x05 => Some(Self::Message),
            0x06 => Some(Self::Chunk),
            0x07 => Some(Self::MessageIndex),
            0x08 => Some(Self::ChunkIndex),
            0x09 => Some(Self::Attachment),
            0x0a => Some(Self::AttachmentIndex),
            0x0b => Some(Self::Statistics),
            0x0c => Some(Self::Metadata),
            0x0d => Some(Self::MetadataIndex),
            0x0e => Some(Self::SummaryOffset),
            0x0f => Some(Self::DataEnd),
            _ => None,
        }
    }

    /// The wire byte this opcode was parsed from.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Header => 0x01,
            Self::Footer => 0x02,
            Self::Schema => 0x03,
            Self::Channel => 0x04,
            Self::Message => 0x05,
            Self::Chunk => 0x06,
            Self::MessageIndex => 0x07,
            Self::ChunkIndex => 0x08,
            Self::Attachment => 0x09,
            Self::AttachmentIndex => 0x0a,
            Self::Statistics => 0x0b,
            Self::Metadata => 0x0c,
            Self::MetadataIndex => 0x0d,
            Self::SummaryOffset => 0x0e,
            Self::DataEnd => 0x0f,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_opcode_round_trips_through_its_byte() {
        let all = [
            Opcode::Header,
            Opcode::Footer,
            Opcode::Schema,
            Opcode::Channel,
            Opcode::Message,
            Opcode::Chunk,
            Opcode::MessageIndex,
            Opcode::ChunkIndex,
            Opcode::Attachment,
            Opcode::AttachmentIndex,
            Opcode::Statistics,
            Opcode::Metadata,
            Opcode::MetadataIndex,
            Opcode::SummaryOffset,
            Opcode::DataEnd,
        ];
        for opcode in all {
            assert_eq!(Opcode::from_u8(opcode.as_u8()), Some(opcode));
        }
    }

    #[test]
    fn zero_and_reserved_ranges_are_not_valid_opcodes() {
        assert_eq!(Opcode::from_u8(0x00), None);
        assert_eq!(Opcode::from_u8(0x10), None);
        assert_eq!(Opcode::from_u8(0x7f), None);
        assert_eq!(Opcode::from_u8(0x80), None);
        assert_eq!(Opcode::from_u8(0xff), None);
    }
}
