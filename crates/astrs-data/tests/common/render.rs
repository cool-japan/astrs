//! Canonical text rendering of a decoded Arrow stream — the conformance
//! oracle for the golden vectors under `tests/golden/arrow/`.
//!
//! The exact same rendering is implemented against **arrow-rs** in the
//! out-of-workspace generator (`arrow-golden-gen/src/render.rs`), which is
//! what produced every `.expect` file. A byte-identical rendering therefore
//! proves the two implementations agree on every type, value and null of a
//! stream — not just that our decoder did not crash.
//!
//! Grammar (LF-terminated lines):
//!
//! ```text
//! schema fields=<N>
//! meta <key>=<value>            (schema metadata, sorted by key, 0..n lines)
//! field <i> name=<name> type=<T> nullable=<0|1>
//! batch <i> rows=<N> cols=<N>
//! col <i> nulls=<null_count>
//! r<row> <value>
//! end
//! ```
//!
//! Two details are load-bearing:
//!
//! * rows are iterated over the **column's** length, not the batch's row
//!   count — they differ for a zero-column batch, which still declares rows;
//! * `nulls=` is the **physical** null count, i.e. the unset bits of the
//!   validity bitmap. It is `0` for a `Null` column, which has no bitmap at
//!   all, matching what arrow-rs's `Array::null_count` reports. Every slot of
//!   that column still renders as `-`, so nothing is hidden.

#![allow(dead_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fmt::Write as _;

use astrs_data::array::{
    Array, ArrayExt, BooleanArray, DurationArray, FixedSizeBinaryArray, FixedSizeListArray,
    Float16Array, Float32Array, Float64Array, GenericBinaryArray, GenericStringArray, Int8Array,
    Int16Array, Int32Array, Int64Array, ListArray, StructArray, TimestampArray, UInt8Array,
    UInt16Array, UInt32Array, UInt64Array,
};
use astrs_data::{Bitmap, DataType, Field, RecordBatch, Schema};

/// Canonical type string.
pub fn render_type(data_type: &DataType) -> String {
    match data_type {
        DataType::Null => "null".into(),
        DataType::Bool => "bool".into(),
        DataType::Int8 => "i8".into(),
        DataType::Int16 => "i16".into(),
        DataType::Int32 => "i32".into(),
        DataType::Int64 => "i64".into(),
        DataType::UInt8 => "u8".into(),
        DataType::UInt16 => "u16".into(),
        DataType::UInt32 => "u32".into(),
        DataType::UInt64 => "u64".into(),
        DataType::Float16 => "f16".into(),
        DataType::Float32 => "f32".into(),
        DataType::Float64 => "f64".into(),
        DataType::Binary => "binary".into(),
        DataType::LargeBinary => "largebinary".into(),
        DataType::Utf8 => "utf8".into(),
        DataType::LargeUtf8 => "largeutf8".into(),
        DataType::FixedSizeBinary(width) => format!("fsb({width})"),
        DataType::FixedSizeList(field, size) => format!("fsl({},{size})", render_field(field)),
        DataType::List(field) => format!("list({})", render_field(field)),
        DataType::Struct(fields) => {
            let inner: Vec<String> = fields.iter().map(render_field).collect();
            format!("struct({})", inner.join(","))
        }
        DataType::Timestamp => "timestamp(ns)".into(),
        DataType::Duration => "duration(ns)".into(),
        other => panic!("type outside the closed AstRS set: {other:?}"),
    }
}

/// Canonical `<name>:<type>:<nullable>` child-field string.
pub fn render_field(field: &Field) -> String {
    format!(
        "{}:{}:{}",
        field.name(),
        render_type(field.data_type()),
        u8::from(field.is_nullable())
    )
}

/// Lower-case hex, two digits per byte.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The physical null count: unset bits of the validity bitmap, `0` when there
/// is none.
pub fn physical_null_count(array: &dyn Array) -> usize {
    array.validity().map_or(0, Bitmap::count_unset)
}

/// Canonical value rendering for row `index` of `array`.
pub fn render_value(array: &dyn Array, index: usize) -> String {
    if array.is_null(index) {
        return "-".into();
    }
    match array.data_type() {
        DataType::Null => "-".into(),
        DataType::Bool => {
            let typed = array.downcast::<BooleanArray>().expect("bool");
            if typed.value(index) == Some(true) {
                "T".into()
            } else {
                "F".into()
            }
        }
        DataType::Int8 => scalar(array.downcast::<Int8Array>().expect("i8").value(index)),
        DataType::Int16 => scalar(array.downcast::<Int16Array>().expect("i16").value(index)),
        DataType::Int32 => scalar(array.downcast::<Int32Array>().expect("i32").value(index)),
        DataType::Int64 => scalar(array.downcast::<Int64Array>().expect("i64").value(index)),
        DataType::UInt8 => scalar(array.downcast::<UInt8Array>().expect("u8").value(index)),
        DataType::UInt16 => scalar(array.downcast::<UInt16Array>().expect("u16").value(index)),
        DataType::UInt32 => scalar(array.downcast::<UInt32Array>().expect("u32").value(index)),
        DataType::UInt64 => scalar(array.downcast::<UInt64Array>().expect("u64").value(index)),
        DataType::Float16 => {
            let value = array
                .downcast::<Float16Array>()
                .expect("f16")
                .value(index)
                .expect("value");
            format!("h{:04x}", value.to_bits())
        }
        DataType::Float32 => {
            let value = array
                .downcast::<Float32Array>()
                .expect("f32")
                .value(index)
                .expect("value");
            format!("s{:08x}", value.to_bits())
        }
        DataType::Float64 => {
            let value = array
                .downcast::<Float64Array>()
                .expect("f64")
                .value(index)
                .expect("value");
            format!("d{:016x}", value.to_bits())
        }
        DataType::Timestamp => scalar(
            array
                .downcast::<TimestampArray>()
                .expect("timestamp")
                .value(index),
        ),
        DataType::Duration => scalar(
            array
                .downcast::<DurationArray>()
                .expect("duration")
                .value(index),
        ),
        DataType::Binary => {
            let typed = array.downcast::<GenericBinaryArray<i32>>().expect("binary");
            format!("x{}", hex(typed.value(index).expect("value")))
        }
        DataType::LargeBinary => {
            let typed = array
                .downcast::<GenericBinaryArray<i64>>()
                .expect("largebinary");
            format!("x{}", hex(typed.value(index).expect("value")))
        }
        DataType::FixedSizeBinary(_) => {
            let typed = array.downcast::<FixedSizeBinaryArray>().expect("fsb");
            format!("x{}", hex(typed.value(index).expect("value")))
        }
        DataType::Utf8 => {
            let typed = array.downcast::<GenericStringArray<i32>>().expect("utf8");
            format!("u{}", hex(typed.value(index).expect("value").as_bytes()))
        }
        DataType::LargeUtf8 => {
            let typed = array
                .downcast::<GenericStringArray<i64>>()
                .expect("largeutf8");
            format!("u{}", hex(typed.value(index).expect("value").as_bytes()))
        }
        DataType::List(_) => {
            let typed = array.downcast::<ListArray>().expect("list");
            render_seq(typed.value(index).expect("value").as_ref())
        }
        DataType::FixedSizeList(_, _) => {
            let typed = array.downcast::<FixedSizeListArray>().expect("fsl");
            render_seq(typed.value(index).expect("value").as_ref())
        }
        DataType::Struct(fields) => {
            let typed = array.downcast::<StructArray>().expect("struct");
            let mut out = String::from("{");
            for (position, field) in fields.iter().enumerate() {
                if position > 0 {
                    out.push(' ');
                }
                let _ = write!(out, "{}=", field.name());
                let column = typed.column(position).expect("column");
                out.push_str(&render_value(column.as_ref(), index));
            }
            out.push('}');
            out
        }
        other => panic!("type outside the closed AstRS set: {other:?}"),
    }
}

/// A scalar that is always present at this point (the null check ran first).
fn scalar<T: std::fmt::Display>(value: Option<T>) -> String {
    match value {
        Some(value) => format!("{value}"),
        None => "-".into(),
    }
}

/// `[a b c]` over every element of a child slice.
fn render_seq(child: &dyn Array) -> String {
    let mut out = String::from("[");
    for index in 0..child.len() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(&render_value(child, index));
    }
    out.push(']');
    out
}

/// Canonical rendering of a whole stream (schema plus batches).
pub fn render_stream(schema: &Schema, batches: &[RecordBatch]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "schema fields={}", schema.len());
    for (key, value) in schema.metadata() {
        let _ = writeln!(out, "meta {key}={value}");
    }
    for (index, field) in schema.fields().iter().enumerate() {
        let _ = writeln!(
            out,
            "field {index} name={} type={} nullable={}",
            field.name(),
            render_type(field.data_type()),
            u8::from(field.is_nullable())
        );
    }
    for (batch_index, batch) in batches.iter().enumerate() {
        let _ = writeln!(
            out,
            "batch {batch_index} rows={} cols={}",
            batch.num_rows(),
            batch.num_columns()
        );
        for (column_index, column) in batch.columns().iter().enumerate() {
            let _ = writeln!(
                out,
                "col {column_index} nulls={}",
                physical_null_count(column.as_ref())
            );
            for row in 0..column.len() {
                let _ = writeln!(out, "r{row} {}", render_value(column.as_ref(), row));
            }
        }
    }
    out.push_str("end\n");
    out
}

/// The first line at which two renderings differ, for readable failures.
pub fn first_difference(expected: &str, actual: &str) -> String {
    let mut left = expected.lines();
    let mut right = actual.lines();
    let mut line = 0usize;
    loop {
        line += 1;
        match (left.next(), right.next()) {
            (None, None) => return "identical line by line (trailing bytes differ)".into(),
            (Some(a), Some(b)) if a == b => continue,
            (a, b) => {
                return format!(
                    "line {line}: expected {:?}, decoded {:?}",
                    a.unwrap_or("<eof>"),
                    b.unwrap_or("<eof>")
                );
            }
        }
    }
}
