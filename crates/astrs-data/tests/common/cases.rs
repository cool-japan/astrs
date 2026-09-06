//! The synthetic IPC corpus: one named [`Case`] per shape the AstRS writer
//! has to get right, built with the public builders and array constructors.
//!
//! The golden vectors under `tests/golden/arrow/` cover the *read* direction —
//! bytes arrow-rs produced, decoded here. This corpus covers the *write*
//! direction: batches AstRS builds, encoded here, and read back both by
//! `astrs_data::ipc` (`tests/ipc_roundtrip.rs`) and by arrow-rs itself
//! (`tests/cross_validate.rs` exports them for the out-of-workspace harness).
//!
//! Between them the two corpora close the loop the blueprint's risk register
//! §23 #1 asks for: arrow-rs bytes in, AstRS bytes out, both directions
//! value-for-value.
//!
//! Every entry of the closed §6.1 type set appears in at least one case, and
//! the awkward shapes are deliberate: zero rows, zero columns, all-null
//! columns, sliced (non-zero offset) columns that force the writer to re-base,
//! nesting three levels deep, a stream of several batches, a stream of none,
//! and a row count large enough that buffer padding stops being incidental.

#![allow(dead_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use astrs_data::array::{
    ArrayRef, BinaryArray, BooleanArray, DurationArray, FixedSizeListArray, Float16Array,
    Float32Array, Float64Array, Int8Array, Int16Array, Int32Array, Int64Array, IntoArrayRef,
    LargeBinaryArray, LargeStringArray, ListArray, NullArray, StringArray, StructArray,
    TimestampArray, UInt8Array, UInt16Array, UInt32Array, UInt64Array,
};
use astrs_data::builder::{BinaryBuilder, FixedSizeBinaryBuilder};
use astrs_data::{Array, Bitmap, DataType, F16, Field, RecordBatch, Schema};

/// One named stream: the schema it opens with and the batches it carries.
///
/// A case with no batches is a schema-only stream, which is what a producer
/// that has not published data yet sends.
pub struct Case {
    /// File-safe name, used for the exported `<name>.arrows` artefacts.
    pub name: &'static str,
    /// The stream's schema.
    pub schema: Arc<Schema>,
    /// The batches, all sharing `schema`.
    pub batches: Vec<RecordBatch>,
}

impl Case {
    /// Builds a case from a schema and its batches, checking the invariant the
    /// writer will check anyway: every batch carries that schema.
    fn new(name: &'static str, schema: Arc<Schema>, batches: Vec<RecordBatch>) -> Self {
        for (index, batch) in batches.iter().enumerate() {
            assert_eq!(
                batch.schema().as_ref(),
                schema.as_ref(),
                "{name}: batch {index} has a different schema"
            );
        }
        Self {
            name,
            schema,
            batches,
        }
    }

    /// A single-batch case whose schema is taken from the batch.
    fn single(name: &'static str, batch: RecordBatch) -> Self {
        Self::new(name, batch.schema_ref(), vec![batch])
    }
}

/// The whole corpus, in a stable order.
pub fn cases() -> Vec<Case> {
    vec![
        primitives_no_null(),
        primitives_nulls(),
        primitives_all_null(),
        primitives_empty(),
        float_edge_cases(),
        bytes_types(),
        bytes_empty(),
        null_column(),
        list_i32(),
        list_of_list(),
        list_of_struct(),
        struct_nested(),
        fixed_size_list_tensor(),
        fixed_size_binary(),
        deep_nesting(),
        nested_empty(),
        nested_all_null(),
        multi_batch(),
        schema_only(),
        zero_columns(),
        sliced_columns(),
        wide_rows(),
        dora_payload(),
        schema_metadata(),
    ]
}

/// Looks one case up by name.
pub fn case(name: &str) -> Case {
    cases()
        .into_iter()
        .find(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("no such case: {name}"))
}

/// Builds a batch from `(name, nullable, array)` triples, taking each field's
/// type from the array itself.
fn batch_of(columns: Vec<(&str, bool, ArrayRef)>) -> RecordBatch {
    let fields: Vec<Field> = columns
        .iter()
        .map(|(name, nullable, array)| Field::new(*name, array.data_type().clone(), *nullable))
        .collect();
    let arrays: Vec<ArrayRef> = columns.into_iter().map(|(_, _, array)| array).collect();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).expect("batch")
}

/// A single-column payload batch under the `data` convention of §6.1.
fn payload_batch(array: ArrayRef) -> RecordBatch {
    RecordBatch::from_payload(array)
}

/// Every primitive of the closed set, no nulls anywhere.
fn primitives_no_null() -> Case {
    Case::single("primitives_no_null", batch_of(primitive_columns(false)))
}

/// Every primitive with an interleaved null pattern.
fn primitives_nulls() -> Case {
    Case::single("primitives_nulls", batch_of(primitive_columns(true)))
}

/// The primitive columns, optionally with nulls at rows 1 and 4.
fn primitive_columns(nulls: bool) -> Vec<(&'static str, bool, ArrayRef)> {
    /// Punches nulls into a six-element value list.
    fn opt<T: Copy>(values: [T; 6], nulls: bool) -> Vec<Option<T>> {
        values
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                if nulls && (index == 1 || index == 4) {
                    None
                } else {
                    Some(value)
                }
            })
            .collect()
    }

    vec![
        (
            "bool",
            true,
            BooleanArray::from_opt_iter(opt([true, false, true, true, false, false], nulls))
                .into_array_ref(),
        ),
        (
            "i8",
            true,
            Int8Array::from_opt_iter(opt([i8::MIN, -1, 0, 1, 42, i8::MAX], nulls)).into_array_ref(),
        ),
        (
            "i16",
            true,
            Int16Array::from_opt_iter(opt([i16::MIN, -1, 0, 1, 4242, i16::MAX], nulls))
                .into_array_ref(),
        ),
        (
            "i32",
            true,
            Int32Array::from_opt_iter(opt([i32::MIN, -1, 0, 1, 424_242, i32::MAX], nulls))
                .into_array_ref(),
        ),
        (
            "i64",
            true,
            Int64Array::from_opt_iter(opt([i64::MIN, -1, 0, 1, 424_242_424_242, i64::MAX], nulls))
                .into_array_ref(),
        ),
        (
            "u8",
            true,
            UInt8Array::from_opt_iter(opt([0, 1, 2, 127, 128, u8::MAX], nulls)).into_array_ref(),
        ),
        (
            "u16",
            true,
            UInt16Array::from_opt_iter(opt([0, 1, 2, 32_767, 32_768, u16::MAX], nulls))
                .into_array_ref(),
        ),
        (
            "u32",
            true,
            UInt32Array::from_opt_iter(opt([0, 1, 2, 65_535, 65_536, u32::MAX], nulls))
                .into_array_ref(),
        ),
        (
            "u64",
            true,
            UInt64Array::from_opt_iter(opt(
                [0, 1, 2, 4_294_967_295, 4_294_967_296, u64::MAX],
                nulls,
            ))
            .into_array_ref(),
        ),
        (
            "f16",
            true,
            Float16Array::from_opt_iter(opt(
                [
                    F16::from_f32(-1.5),
                    F16::from_f32(0.0),
                    F16::from_f32(1.0),
                    F16::from_f32(2.5),
                    F16::from_f32(65504.0),
                    F16::from_f32(0.000_061_035_156),
                ],
                nulls,
            ))
            .into_array_ref(),
        ),
        (
            "f32",
            true,
            Float32Array::from_opt_iter(opt(
                [-1.5, 0.0, 1.0, 2.5, f32::MAX, f32::MIN_POSITIVE],
                nulls,
            ))
            .into_array_ref(),
        ),
        (
            "f64",
            true,
            Float64Array::from_opt_iter(opt(
                [-1.5, 0.0, 1.0, 2.5, f64::MAX, f64::MIN_POSITIVE],
                nulls,
            ))
            .into_array_ref(),
        ),
        (
            "ts",
            true,
            TimestampArray::from_opt_nanos(opt(
                [0, 1, 1_700_000_000_000_000_000, -1, i64::MIN, i64::MAX],
                nulls,
            ))
            .into_array_ref(),
        ),
        (
            "dur",
            true,
            DurationArray::from_opt_nanos(opt(
                [0, 1, -1, 1_000_000_000, i64::MIN, i64::MAX],
                nulls,
            ))
            .into_array_ref(),
        ),
    ]
}

/// Every primitive column entirely null — the validity bitmap is all zeros and
/// the value slots are undefined, which a decoder must not read.
fn primitives_all_null() -> Case {
    let columns: Vec<(&str, bool, ArrayRef)> = vec![
        ("bool", true, BooleanArray::new_null(5).into_array_ref()),
        ("i32", true, Int32Array::new_null(5).into_array_ref()),
        ("u64", true, UInt64Array::new_null(5).into_array_ref()),
        ("f64", true, Float64Array::new_null(5).into_array_ref()),
        ("f16", true, Float16Array::new_null(5).into_array_ref()),
        ("ts", true, TimestampArray::new_null(5).into_array_ref()),
        ("dur", true, DurationArray::new_null(5).into_array_ref()),
        ("utf8", true, StringArray::new_null(5).into_array_ref()),
        ("bin", true, BinaryArray::new_null(5).into_array_ref()),
    ];
    Case::single("primitives_all_null", batch_of(columns))
}

/// Every primitive with zero rows: buffers are empty, offsets still have their
/// leading zero, and the message body is (almost) all padding.
fn primitives_empty() -> Case {
    let columns: Vec<(&str, bool, ArrayRef)> = vec![
        ("bool", true, BooleanArray::from_values([]).into_array_ref()),
        ("i32", false, Int32Array::from_values([]).into_array_ref()),
        ("f64", true, Float64Array::from_values([]).into_array_ref()),
        (
            "utf8",
            true,
            StringArray::from_values::<&str>([]).into_array_ref(),
        ),
        (
            "bin",
            true,
            BinaryArray::from_values::<&[u8]>([]).into_array_ref(),
        ),
        ("ts", true, TimestampArray::from_nanos([]).into_array_ref()),
    ];
    Case::single("primitives_empty", batch_of(columns))
}

/// Float bit patterns that must survive verbatim: NaN payloads, both zeroes,
/// both infinities and the subnormal edge.
fn float_edge_cases() -> Case {
    let f32_values = [
        f32::NAN,
        -f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        0.0,
        -0.0,
        f32::MIN_POSITIVE,
        f32::from_bits(1),
    ];
    let f64_values = [
        f64::NAN,
        -f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.0,
        -0.0,
        f64::MIN_POSITIVE,
        f64::from_bits(1),
    ];
    let f16_values = [
        F16::from_bits(0x7e00),
        F16::from_bits(0xfe00),
        F16::from_bits(0x7c00),
        F16::from_bits(0xfc00),
        F16::from_bits(0x0000),
        F16::from_bits(0x8000),
        F16::from_bits(0x0400),
        F16::from_bits(0x0001),
    ];
    Case::single(
        "float_edge_cases",
        batch_of(vec![
            (
                "f32",
                false,
                Float32Array::from_values(f32_values).into_array_ref(),
            ),
            (
                "f64",
                false,
                Float64Array::from_values(f64_values).into_array_ref(),
            ),
            (
                "f16",
                false,
                Float16Array::from_values(f16_values).into_array_ref(),
            ),
        ]),
    )
}

/// The four variable-length families plus fixed-size binary, with nulls, empty
/// values and multi-byte UTF-8.
fn bytes_types() -> Case {
    let mut binary = BinaryBuilder::new();
    binary.append_value([0u8, 1, 2, 255]);
    binary.append_value([] as [u8; 0]);
    binary.append_option(None::<&[u8]>);
    binary.append_value([0xde, 0xad, 0xbe, 0xef, 0x00]);

    let mut fixed = FixedSizeBinaryBuilder::new(3).expect("width");
    fixed.append_value([1u8, 2, 3]).expect("append");
    fixed.append_option(None::<&[u8]>).expect("append");
    fixed.append_value([0u8, 0, 0]).expect("append");
    fixed.append_value([255u8, 254, 253]).expect("append");

    Case::single(
        "bytes_types",
        batch_of(vec![
            (
                "utf8",
                true,
                StringArray::from_opt_iter([
                    Some("astrs"),
                    Some(""),
                    None,
                    Some("日本語 · ロボット"),
                ])
                .into_array_ref(),
            ),
            (
                "largeutf8",
                true,
                LargeStringArray::from_opt_iter([
                    Some("large"),
                    None,
                    Some(""),
                    Some("\u{1f680}\u{0}tail"),
                ])
                .into_array_ref(),
            ),
            ("binary", true, binary.finish().into_array_ref()),
            (
                "largebinary",
                true,
                LargeBinaryArray::from_opt_iter([
                    Some(vec![1u8]),
                    Some(vec![]),
                    None,
                    Some(vec![7u8; 40]),
                ])
                .into_array_ref(),
            ),
            ("fsb3", true, fixed.finish().into_array_ref()),
        ]),
    )
}

/// Variable-length columns with zero rows: the offsets buffer still carries
/// its single leading zero, and the values buffer is empty.
fn bytes_empty() -> Case {
    Case::single(
        "bytes_empty",
        batch_of(vec![
            (
                "utf8",
                true,
                StringArray::from_values::<&str>([]).into_array_ref(),
            ),
            (
                "largeutf8",
                true,
                LargeStringArray::from_values::<&str>([]).into_array_ref(),
            ),
            (
                "binary",
                true,
                BinaryArray::from_values::<&[u8]>([]).into_array_ref(),
            ),
            (
                "fsb4",
                true,
                FixedSizeBinaryBuilder::new(4)
                    .expect("width")
                    .finish()
                    .into_array_ref(),
            ),
        ]),
    )
}

/// The `Null` type: a column with no buffers at all, beside a normal one.
fn null_column() -> Case {
    Case::single(
        "null_column",
        batch_of(vec![
            ("n", true, NullArray::new(4).into_array_ref()),
            (
                "i32",
                true,
                Int32Array::from_opt_iter([Some(1), None, Some(3), Some(4)]).into_array_ref(),
            ),
        ]),
    )
}

/// `List<Int32>` with a null list, an empty list and a long one.
fn list_i32() -> Case {
    let values = Int32Array::from_opt_iter([
        Some(1),
        Some(2),
        Some(3),
        None,
        Some(5),
        Some(6),
        Some(7),
        Some(8),
    ])
    .into_array_ref();
    let offsets: Vec<i32> = vec![0, 3, 3, 4, 8];
    let validity = Bitmap::from_iter([true, true, false, true]);
    let array = ListArray::try_new(
        Field::new("item", DataType::Int32, true),
        offsets.into(),
        values,
        Some(validity),
    )
    .expect("list");
    Case::single(
        "list_i32",
        batch_of(vec![("l", true, array.into_array_ref())]),
    )
}

/// `List<List<UInt8>>` — two levels of offsets, which is where an off-by-one
/// in re-basing shows up first.
fn list_of_list() -> Case {
    let inner_values = UInt8Array::from_values([1u8, 2, 3, 4, 5, 6]).into_array_ref();
    let inner = ListArray::try_new(
        Field::new("item", DataType::UInt8, false),
        vec![0i32, 2, 2, 5, 6].into(),
        inner_values,
        None,
    )
    .expect("inner list");
    let outer = ListArray::try_new(
        Field::new(
            "item",
            DataType::list(Field::new("item", DataType::UInt8, false)),
            true,
        ),
        vec![0i32, 2, 3, 4].into(),
        inner.into_array_ref(),
        Some(Bitmap::from_iter([true, false, true])),
    )
    .expect("outer list");
    Case::single(
        "list_of_list",
        batch_of(vec![("ll", true, outer.into_array_ref())]),
    )
}

/// `List<Struct<..>>` — the shape the blueprint names explicitly, and the one
/// that mixes offsets with a struct's per-row validity.
fn list_of_struct() -> Case {
    let ids = UInt32Array::from_values([10u32, 11, 12, 13, 14]).into_array_ref();
    let scores =
        Float32Array::from_opt_iter([Some(0.5), None, Some(1.5), Some(2.5), None]).into_array_ref();
    let labels = StringArray::from_opt_iter([Some("a"), Some("b"), None, Some(""), Some("e")])
        .into_array_ref();
    let struct_fields = vec![
        Field::new("id", DataType::UInt32, false),
        Field::new("score", DataType::Float32, true),
        Field::new("label", DataType::Utf8, true),
    ];
    let child = StructArray::try_new(
        struct_fields.clone(),
        vec![ids, scores, labels],
        Some(Bitmap::from_iter([true, true, false, true, true])),
    )
    .expect("struct");
    let list = ListArray::try_new(
        Field::new("item", DataType::strukt(struct_fields), true),
        vec![0i32, 2, 2, 5].into(),
        child.into_array_ref(),
        Some(Bitmap::from_iter([true, true, false])),
    )
    .expect("list of struct");
    Case::single(
        "list_of_struct",
        batch_of(vec![("detections", true, list.into_array_ref())]),
    )
}

/// A struct holding a struct, with nulls at both levels.
fn struct_nested() -> Case {
    let inner_fields = vec![
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ];
    let inner = StructArray::try_new(
        inner_fields.clone(),
        vec![
            Float64Array::from_values([1.0, 2.0, 3.0]).into_array_ref(),
            Float64Array::from_values([-1.0, -2.0, -3.0]).into_array_ref(),
        ],
        None,
    )
    .expect("inner struct");
    let outer_fields = vec![
        Field::new("id", DataType::UInt32, true),
        Field::new("p", DataType::strukt(inner_fields), false),
        Field::new("label", DataType::Utf8, true),
    ];
    let outer = StructArray::try_new(
        outer_fields,
        vec![
            UInt32Array::from_opt_iter([Some(1), Some(2), None]).into_array_ref(),
            inner.into_array_ref(),
            StringArray::from_opt_iter([Some("a"), None, Some("c")]).into_array_ref(),
        ],
        Some(Bitmap::from_iter([true, false, true])),
    )
    .expect("outer struct");
    Case::single(
        "struct_nested",
        batch_of(vec![("s", true, outer.into_array_ref())]),
    )
}

/// `FixedSizeList<Float32, 12>` — a 3×4 tensor per row, the layout the tensor
/// views and the image payloads are built on.
fn fixed_size_list_tensor() -> Case {
    let values: Vec<f32> = (0..48).map(|value| value as f32 * 0.25).collect();
    let array = FixedSizeListArray::try_new(
        Field::new("item", DataType::Float32, false),
        12,
        Float32Array::from_values(values).into_array_ref(),
        Some(Bitmap::from_iter([true, false, true, true])),
    )
    .expect("fixed size list");
    Case::single(
        "fixed_size_list_tensor",
        batch_of(vec![("tensor", true, array.into_array_ref())]),
    )
}

/// Fixed-size binary at three widths, including the single-byte edge.
fn fixed_size_binary() -> Case {
    let mut one = FixedSizeBinaryBuilder::new(1).expect("width");
    for value in [0u8, 1, 128, 255] {
        one.append_value([value]).expect("append");
    }
    let mut sixteen = FixedSizeBinaryBuilder::new(16).expect("width");
    for row in 0..4u8 {
        sixteen.append_value([row; 16]).expect("append");
    }
    let mut seven = FixedSizeBinaryBuilder::new(7).expect("width");
    seven.append_value(b"astrs!!").expect("append");
    seven.append_option(None::<&[u8]>).expect("append");
    seven.append_value([0u8; 7]).expect("append");
    seven
        .append_value(b"\xff\x00\xff\x00\xff\x00\xff")
        .expect("append");

    Case::single(
        "fixed_size_binary",
        batch_of(vec![
            ("w1", false, one.finish().into_array_ref()),
            ("w16", false, sixteen.finish().into_array_ref()),
            ("w7", true, seven.finish().into_array_ref()),
        ]),
    )
}

/// `List<FixedSizeList<Struct<List<Int64>>>>` — four levels, which exercises
/// the depth-first node and buffer ordering harder than any real payload.
fn deep_nesting() -> Case {
    let leaf = Int64Array::from_values([1i64, 2, 3, 4, 5, 6, 7, 8]).into_array_ref();
    let leaf_list = ListArray::try_new(
        Field::new("item", DataType::Int64, false),
        vec![0i32, 2, 4, 6, 8].into(),
        leaf,
        None,
    )
    .expect("leaf list");
    let struct_fields = vec![Field::new(
        "values",
        DataType::list(Field::new("item", DataType::Int64, false)),
        true,
    )];
    let structs = StructArray::try_new(
        struct_fields.clone(),
        vec![leaf_list.into_array_ref()],
        Some(Bitmap::from_iter([true, true, false, true])),
    )
    .expect("structs");
    let fsl = FixedSizeListArray::try_new(
        Field::new("item", DataType::strukt(struct_fields), true),
        2,
        structs.into_array_ref(),
        None,
    )
    .expect("fixed size list");
    let outer = ListArray::try_new(
        Field::new("item", fsl.data_type().clone(), true),
        vec![0i32, 1, 2].into(),
        fsl.into_array_ref(),
        Some(Bitmap::from_iter([true, false])),
    )
    .expect("outer list");
    Case::single(
        "deep_nesting",
        batch_of(vec![("deep", true, outer.into_array_ref())]),
    )
}

/// Nested columns with zero rows: a list whose offsets hold only the leading
/// zero, a struct with empty children, an empty fixed-size list.
fn nested_empty() -> Case {
    let list = ListArray::try_new(
        Field::new("item", DataType::Int32, true),
        vec![0i32].into(),
        Int32Array::from_values([]).into_array_ref(),
        None,
    )
    .expect("empty list");
    let structs = StructArray::try_new(
        vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Utf8, true),
        ],
        vec![
            Int32Array::from_values([]).into_array_ref(),
            StringArray::from_values::<&str>([]).into_array_ref(),
        ],
        None,
    )
    .expect("empty struct");
    let fsl = FixedSizeListArray::try_new(
        Field::new("item", DataType::UInt8, false),
        4,
        UInt8Array::from_values([]).into_array_ref(),
        None,
    )
    .expect("empty fsl");
    Case::single(
        "nested_empty",
        batch_of(vec![
            ("list", true, list.into_array_ref()),
            ("struct", true, structs.into_array_ref()),
            ("fsl", true, fsl.into_array_ref()),
            ("null", true, NullArray::new(0).into_array_ref()),
        ]),
    )
}

/// Nested columns where every row is null — the children still exist and are
/// still described by their own field nodes.
fn nested_all_null() -> Case {
    let list = ListArray::new_null(Field::new("item", DataType::Float32, true), 3);
    let structs = StructArray::new_null(
        vec![
            Field::new("a", DataType::Int16, true),
            Field::new("b", DataType::Float64, true),
        ],
        3,
    )
    .expect("null struct");
    let fsl = FixedSizeListArray::new_null(Field::new("item", DataType::UInt32, true), 2, 3)
        .expect("null fsl");
    Case::single(
        "nested_all_null",
        batch_of(vec![
            ("list", true, list.into_array_ref()),
            ("struct", true, structs.into_array_ref()),
            ("fsl", true, fsl.into_array_ref()),
        ]),
    )
}

/// Four batches under one schema, including a zero-row batch in the middle —
/// a producer that emitted nothing for a tick still has to frame a message.
fn multi_batch() -> Case {
    let schema = Arc::new(Schema::new(vec![
        Field::new("seq", DataType::UInt64, false),
        Field::new("label", DataType::Utf8, true),
    ]));
    let mut batches = Vec::new();
    for (index, rows) in [3usize, 0, 1, 5].into_iter().enumerate() {
        let base = (index * 100) as u64;
        let seq = UInt64Array::from_values((0..rows as u64).map(|row| base + row)).into_array_ref();
        let label = StringArray::from_opt_iter((0..rows).map(|row| {
            if row % 2 == 0 {
                Some(format!("batch{index}-row{row}"))
            } else {
                None
            }
        }))
        .into_array_ref();
        batches.push(
            RecordBatch::try_new(Arc::clone(&schema), vec![seq, label]).expect("multi batch"),
        );
    }
    Case::new("multi_batch", schema, batches)
}

/// A stream with a schema and no batches at all.
fn schema_only() -> Case {
    let schema = Arc::new(Schema::new(vec![
        Field::new("a", DataType::Int32, true),
        Field::new("b", DataType::Utf8, false),
        Field::new(
            "c",
            DataType::strukt(vec![Field::new("inner", DataType::Float32, true)]),
            true,
        ),
    ]));
    Case::new("schema_only", schema, Vec::new())
}

/// A batch with no columns but a positive row count — legal, and a shape a
/// naive writer loses.
fn zero_columns() -> Case {
    let schema = Arc::new(Schema::new(Vec::new()));
    let batch = RecordBatch::try_new_with_row_count(Arc::clone(&schema), Vec::new(), 5)
        .expect("zero column batch");
    Case::new("zero_columns", schema, vec![batch])
}

/// Columns sliced away from offset zero, which the writer must re-base: a
/// primitive, a bitmap-backed boolean, a string with non-zero offsets, a list
/// and a struct.
fn sliced_columns() -> Case {
    let ints = Int32Array::from_opt_iter((0..10).map(|value| {
        if value % 3 == 0 {
            None
        } else {
            Some(value * 7)
        }
    }));
    let flags = BooleanArray::from_values((0..10).map(|value| value % 2 == 0));
    let strings = StringArray::from_opt_iter((0..10).map(|value| {
        if value == 4 {
            None
        } else {
            Some(format!("row-{value}"))
        }
    }));
    let list_values = Int32Array::from_values(0..20).into_array_ref();
    let list = ListArray::try_new(
        Field::new("item", DataType::Int32, false),
        (0..=10).map(|value| value * 2).collect::<Vec<i32>>().into(),
        list_values,
        None,
    )
    .expect("list");
    let structs = StructArray::try_new(
        vec![
            Field::new("a", DataType::UInt8, false),
            Field::new("b", DataType::Float64, true),
        ],
        vec![
            UInt8Array::from_values((0..10).map(|value| value as u8)).into_array_ref(),
            Float64Array::from_opt_iter((0..10).map(|value| {
                if value == 7 {
                    None
                } else {
                    Some(f64::from(value) / 4.0)
                }
            }))
            .into_array_ref(),
        ],
        Some(Bitmap::from_iter((0..10).map(|value| value != 5))),
    )
    .expect("struct");

    Case::single(
        "sliced_columns",
        batch_of(vec![
            ("ints", true, ints.slice(3, 5).into_array_ref()),
            ("flags", false, flags.slice(3, 5).into_array_ref()),
            ("strings", true, strings.slice(3, 5).into_array_ref()),
            ("list", true, list.slice(3, 5).into_array_ref()),
            ("struct", true, structs.slice(3, 5).into_array_ref()),
        ]),
    )
}

/// Enough rows that every buffer crosses several padding boundaries.
fn wide_rows() -> Case {
    const ROWS: usize = 4096;
    let ints = Int64Array::from_opt_iter((0..ROWS).map(|row| {
        if row % 97 == 0 {
            None
        } else {
            Some(row as i64 * 1_000_003)
        }
    }));
    let text = StringArray::from_values((0..ROWS).map(|row| format!("sample-{row:05}")));
    let bytes = FixedSizeListArray::try_new(
        Field::new("item", DataType::UInt8, false),
        3,
        UInt8Array::from_values((0..ROWS * 3).map(|value| (value % 251) as u8)).into_array_ref(),
        None,
    )
    .expect("fsl");
    Case::single(
        "wide_rows",
        batch_of(vec![
            ("ints", true, ints.into_array_ref()),
            ("text", false, text.into_array_ref()),
            ("rgb", false, bytes.into_array_ref()),
        ]),
    )
}

/// The §6.1 payload convention: one batch, one top-level column named `data`,
/// here a 1024-element `FixedSizeList<Float32, 4>` tensor.
fn dora_payload() -> Case {
    const ROWS: usize = 1024;
    let values: Vec<f32> = (0..ROWS * 4).map(|value| value as f32 * 0.5).collect();
    let array = FixedSizeListArray::try_new(
        Field::new("item", DataType::Float32, false),
        4,
        Float32Array::from_values(values).into_array_ref(),
        None,
    )
    .expect("payload tensor");
    Case::single("dora_payload", payload_batch(array.into_array_ref()))
}

/// Schema-level metadata, including the empty value and the reserved
/// `_schema_hash` key AstRS stamps.
fn schema_metadata() -> Case {
    let schema = Arc::new(
        Schema::new(vec![Field::new("data", DataType::Float32, false)])
            .with_metadata_entry("std/type", "std/core/v1/Float32")
            .with_metadata_entry("_schema_hash", "0123456789abcdef")
            .with_metadata_entry("empty", "")
            .with_metadata_entry("unicode", "カメラ"),
    );
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![Float32Array::from_values([1.0, 2.0]).into_array_ref()],
    )
    .expect("metadata batch");
    Case::new("schema_metadata", schema, vec![batch])
}
