# astrs-data

The AstRS columnar payload format — Arrow IPC wire-compatible, natively
implemented.

Every payload on every AstRS leg is a self-describing Arrow IPC stream
readable by arrow-rs, pyarrow and arrow-cpp, encoded here with no `arrow`,
`flatbuffers` or other foreign dependency: 64-byte aligned buffers and
validity bitmaps (so a mapped shared-memory payload is directly usable as a
SIMD source), the closed P0 array set (`Null`/`Bool`/`Int8..64`/
`UInt8..64`/`Float16..64`/`Binary`/`LargeBinary`/`Utf8`/`LargeUtf8`/
`FixedSizeBinary`/`FixedSizeList`/`List`/`Struct`/`Timestamp`/`Duration`),
`RecordBatch` plus the `std` type URN registry that says what a port's
declared type actually means, schema hashing (in-crate XXH3-64) so
receivers cache decoded schemas cheaply, `TensorView`/`ImageView` for
checked N-D access, compute kernels (slice/concat/cast/take/filter), and
the IPC wire encoding itself — a from-scratch FlatBuffers codec, gated on
byte-compatibility golden vectors under `tests/golden/arrow/`, not on
inspection.

The optional `arrow-interop` feature (off by default) adds zero-copy
`From`/`TryFrom` conversions between this crate's arrays and `arrow-rs`
59's own types, for a caller embedding AstRS into an already-arrow-native
pipeline (DataFusion, Polars-via-arrow, Parquet, a pyarrow boundary) rather
than talking to another AstRS node; the default build pulls in none of
`arrow-array`/`arrow-buffer`/`arrow-data`/`arrow-schema`.

## Example

```rust
use astrs_data::builder::{ArrayBuilder, Int32Builder, StringBuilder};
use astrs_data::{DataType, Field, RecordBatch, Schema};
use std::sync::Arc;

let mut ids = Int32Builder::with_capacity(3);
ids.append_value(7);
ids.append_null();
ids.append_value(9);

let mut names = StringBuilder::with_capacity(3, 16);
names.append_value("lidar");
names.append_value("camera");
names.append_null();

let schema = Arc::new(Schema::new(vec![
    Field::new("id", DataType::Int32, true),
    Field::new("name", DataType::Utf8, true),
]));
let batch = RecordBatch::try_new(schema, vec![ids.finish_array(), names.finish_array()])?;

assert_eq!(batch.num_rows(), 3);
assert_eq!(batch.num_columns(), 2);
# Ok::<(), astrs_data::DataError>(())
```

See the [crate documentation](https://docs.rs/astrs-data) for the wire format
and the closed array-type set this crate encodes.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
