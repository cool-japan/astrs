//! A self-contained FlatBuffers codec, sized to Arrow's `Message.fbs` and
//! `Schema.fbs`.
//!
//! AstRS must not depend on the `flatbuffers` crate (blueprint §18.1 bans it),
//! and Arrow's IPC headers are the only flatbuffers AstRS will ever read or
//! write. This module implements exactly the subset those two schemas use:
//!
//! | Feature | Supported | Notes |
//! |---|---|---|
//! | Tables, vtables, default elision | yes | [`FbBuilder`], [`Table`] |
//! | Vtable de-duplication | yes | produces negative `soffset_t`, which the reader follows |
//! | Scalars `bool`/`i8`/`u8`/`i16`/`i32`/`i64` | yes | little-endian only, as the format mandates |
//! | Strings | yes | length-prefixed, NUL-terminated, UTF-8 validated on read |
//! | Vectors of offsets | yes | `[Field]`, `[KeyValue]` |
//! | Vectors of 16-byte structs | yes | `[FieldNode]`, `[Buffer]` |
//! | Unions | yes | as the `(type: u8, value: offset)` slot pair Arrow uses |
//! | Nested/inline structs, 64-bit vectors, file identifiers, shared strings | no | unused by Arrow IPC |
//!
//! The split is deliberate: [`builder`] and [`reader`] know nothing about
//! Arrow, and [`crate::ipc::format`] holds every Arrow-specific constant, so
//! the two can be tested independently.

pub mod builder;
pub mod reader;

pub use crate::ipc::fb::builder::{FbBuilder, TableStart, WipOffset};
pub use crate::ipc::fb::reader::{
    SIZE_UOFFSET, SIZE_VOFFSET, Table, VectorSpan, deref, read_i8, read_i16, read_i32, read_i64,
    read_string, read_u8, read_u16, read_u32, root_table,
};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A round trip through every primitive the Arrow schemas use, in one
    /// table, checked field by field.
    #[test]
    fn arrow_shaped_table_round_trips() {
        let mut b = FbBuilder::new();
        let name = b.create_string("point_cloud");
        let key = b.create_string("std/type");
        let value = b.create_string("std/sensor/v1/PointCloud");
        let kv = {
            let t = b.start_table();
            b.push_slot_offset(0, key);
            b.push_slot_offset(1, value);
            b.end_table(t)
        };
        let metadata = b.create_offset_vector(&[kv]);
        let nodes = b.create_struct_pair_vector(&[(64, 0), (192, 3)]);

        let t = b.start_table();
        b.push_slot_offset(0, name);
        b.push_slot_bool(1, true, false);
        b.push_slot_u8(2, 16, 0);
        b.push_slot_i64(3, 1_048_576, 0);
        b.push_slot_offset(4, metadata);
        b.push_slot_offset(5, nodes);
        let root = b.end_table(t);
        b.finish(root);

        let bytes = b.finished_bytes().to_vec();
        let root = root_table(&bytes).unwrap();
        assert_eq!(root.string(0).unwrap(), Some("point_cloud"));
        assert!(root.bool(1, false).unwrap());
        assert_eq!(root.u8(2, 0).unwrap(), 16);
        assert_eq!(root.i64(3, 0).unwrap(), 1_048_576);

        let md = root.vector(4, SIZE_UOFFSET).unwrap().expect("metadata");
        assert_eq!(md.len(), 1);
        let entry = md.table(0).unwrap();
        assert_eq!(entry.string(0).unwrap(), Some("std/type"));
        assert_eq!(entry.string(1).unwrap(), Some("std/sensor/v1/PointCloud"));

        let nodes = root.vector(5, 16).unwrap().expect("nodes");
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes.struct_pair(1).unwrap(), (192, 3));
    }

    #[test]
    fn every_byte_of_a_finished_buffer_survives_truncation() {
        let mut b = FbBuilder::new();
        let name = b.create_string("truncate me");
        let t = b.start_table();
        b.push_slot_offset(0, name);
        b.push_slot_i64(1, 1 << 40, 0);
        let root = b.end_table(t);
        b.finish(root);
        let bytes = b.finished_bytes().to_vec();

        for cut in 0..bytes.len() {
            if let Ok(table) = root_table(&bytes[..cut]) {
                let _ = table.string(0);
                let _ = table.i64(1, 0);
                let _ = table.vector(0, 16);
            }
        }
    }
}
