//! The AstRS columnar payload format — Arrow IPC wire-compatible, natively
//! implemented.
//!
//! Every payload on every AstRS leg is a self-describing Arrow IPC stream
//! readable by arrow-rs, pyarrow and arrow-cpp, encoded here without any
//! `arrow`, `flatbuffers` or other foreign dependency (blueprint §6.1):
//!
//! - 64-byte aligned buffers ([`AlignedBuf`]) and validity bitmaps, so a
//!   mapped shared-memory payload is directly usable as a SIMD source.
//! - The closed P0 array set: `Null`, `Bool`, `Int8..64`, `UInt8..64`,
//!   `Float16/32/64`, `Binary`, `LargeBinary`, `Utf8`, `LargeUtf8`,
//!   `FixedSizeBinary`, `FixedSizeList`, `List`, `Struct`, `Timestamp(ns)`
//!   and `Duration(ns)`.
//! - [`RecordBatch`] — one batch per message, carrying a single top-level
//!   column named [`DATA_COLUMN`] by convention — plus the [`urn`] type
//!   registry that says what a port's declared type actually means, every
//!   `std/v1` type resolving to a normative layout ([`urn::registry`] for the
//!   scalars, [`urn::layouts`] for `media`/`vision`/`geometry`/`sensor`/`nav`).
//! - [`hash::SchemaHash`] (in-crate XXH3-64, [`hash::xxh3`]) so receivers
//!   cache decoded schemas and detect type drift cheaply, [`tensor::TensorView`]
//!   for checked N-D access over a flat column (with [`tensor::ImageView`]
//!   built on it for `std/media/v1/Image`), and [`kernel`]'s compute kernels
//!   (`slice`/`concat`/`cast`/`take`/`filter`) for the node API, the recorder
//!   and the ROS 2 bridge to build on.
//! - [`ipc`] — the wire encoding itself: the Arrow `Message`/`Schema`/
//!   `RecordBatch` flatbuffer tables, hand-encoded and hand-decoded against a
//!   from-scratch FlatBuffers codec ([`ipc::fb`]), with
//!   [`ipc::encode_payload`]/[`ipc::decode_payload`] for the §6.1 one-batch
//!   payload convention and [`ipc::IpcStreamWriter`]/[`ipc::IpcStreamReader`]
//!   for multi-batch streams. Byte compatibility is gated by the arrow-rs
//!   golden vectors under `tests/golden/arrow/`, not by inspection.
//!
//! # Layer map
//!
//! ```text
//!   ipc          IpcStreamWriter/Reader · encode_payload   Arrow IPC on the wire
//!   kernel       slice · concat · cast · take · filter    compute kernels
//!   tensor       TensorView · ImageView                   N-D views over a flat column
//!   hash         SchemaHash · xxh3                         schema fingerprint
//!   urn          TypeUrn · TypeRegistry · layouts          port types (§24.3)
//!   record_batch RecordBatch                                one batch per message
//!   builder      *Builder                                   row-at-a-time writers
//!   array        Array / ArrayRef  ·  13 array families
//!   datatype     DataType · Field · Schema · F16 · ArrowNativeType
//!   buffer       AlignedBuf · Buffer · ScalarBuffer · Bitmap
//! ```
//!
//! Everything above [`buffer`] is safe code, with two kinds of exception
//! reviewed at their call site rather than banned wholesale (per this
//! crate's workspace lint policy): the raw allocation and typed
//! reinterpretation of bytes confined to [`buffer::aligned`] and
//! [`buffer::scalar`], and — at exactly the same standard, reusing an
//! already-UTF-8-validated byte region instead of paying to re-validate it —
//! every place [`kernel`] rebuilds a `Utf8`/`LargeUtf8` array's offsets
//! without touching the bytes they address: [`kernel::concat()`] re-basing
//! offsets across inputs, and [`kernel::cast()`] widening or narrowing the
//! offset integer width between `Utf8` and `LargeUtf8` (the equivalent
//! `Binary`/`LargeBinary` conversions carry no UTF-8 invariant to preserve,
//! so those go through the ordinary checked constructors instead).
//!
//! # Quick tour
//!
//! ```
//! use astrs_data::builder::{ArrayBuilder, Int32Builder, StringBuilder};
//! use astrs_data::{DataType, Field, RecordBatch, Schema};
//! use std::sync::Arc;
//!
//! let mut ids = Int32Builder::with_capacity(3);
//! ids.append_value(7);
//! ids.append_null();
//! ids.append_value(9);
//!
//! let mut names = StringBuilder::with_capacity(3, 16);
//! names.append_value("lidar");
//! names.append_value("camera");
//! names.append_null();
//!
//! let schema = Arc::new(Schema::new(vec![
//!     Field::new("id", DataType::Int32, true),
//!     Field::new("name", DataType::Utf8, true),
//! ]));
//! let batch = RecordBatch::try_new(schema, vec![ids.finish_array(), names.finish_array()])?;
//!
//! assert_eq!(batch.num_rows(), 3);
//! assert_eq!(batch.num_columns(), 2);
//! # Ok::<(), astrs_data::DataError>(())
//! ```
//!
//! # Panic policy
//!
//! `astrs-data` never panics on data-dependent input. Constructors that can
//! reject their arguments are `try_*` and return [`DataError`]; the
//! convenience constructors that cannot fail are infallible. Out-of-range
//! `slice` requests **clamp** rather than panic — see [`array::Array::slice`]
//! for the single, crate-wide convention. The only aborting paths are
//! allocation failure and capacity overflow inside [`AlignedBuf`], which match
//! the standard library's behaviour for `Vec`.

pub mod array;
pub mod buffer;
pub mod builder;
pub mod datatype;
pub mod error;
pub mod hash;
/// Zero-copy `From`/`TryFrom`-shaped conversions to/from arrow-rs 59 arrays
/// (blueprint §6.1), behind the non-default `arrow-interop` feature. See the
/// module's own docs for the layer map and which direction is zero-copy.
#[cfg(feature = "arrow-interop")]
pub mod interop;
pub mod ipc;
pub mod kernel;
pub mod message;
pub mod record_batch;
pub mod tensor;
pub mod urn;

#[doc(hidden)]
pub mod sealed {
    //! Private supertrait used to close the crate's extension points.
    //!
    //! `Sealed` is public only so that the traits using it as a supertrait can
    //! themselves be public. It carries no items and cannot be implemented
    //! from outside this crate.

    /// Marker preventing downstream implementations of the traits that use it
    /// as a supertrait ([`crate::array::Array`],
    /// [`crate::datatype::ArrowNativeType`],
    /// [`crate::array::OffsetSizeTrait`]).
    pub trait Sealed {}
}

pub use crate::array::{Array, ArrayExt, ArrayRef, IntoArrayRef};
pub use crate::buffer::{ALIGNMENT, AlignedBuf, Bitmap, BitmapBuilder, Buffer, ScalarBuffer};
pub use crate::datatype::{ArrowNativeType, DataType, F16, Field, Schema};
pub use crate::error::{DataError, Result};
pub use crate::hash::SchemaHash;
pub use crate::message::AstrsMessage;
pub use crate::record_batch::{ColumnTypeCheck, RecordBatch, RecordBatchOptions};
pub use crate::urn::{TypeRegistry, TypeUrn, TypeUrnError};

/// The single top-level column name AstRS payloads use (blueprint §6.1).
///
/// One record batch per message, one top-level column named `"data"` — the
/// dora convention, kept so dora tooling keeps working against AstRS streams.
pub const DATA_COLUMN: &str = "data";

/// Hard cap on the encoded size of one payload, in bytes (blueprint §6.1).
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;

/// The common imports for code that builds and reads AstRS payloads.
///
/// Everything a node needs to produce or consume a payload, and nothing that
/// only the encoder and the kernels need: the [`Array`] and
/// [`ArrayBuilder`](builder::ArrayBuilder) traits, every concrete builder, the
/// type vocabulary, the error type and the URN entry points.
///
/// ```
/// use astrs_data::prelude::*;
///
/// let mut b = Float64Builder::with_capacity(2);
/// b.append_value(1.5);
/// b.append_null();
/// let array = b.finish();
/// assert_eq!(array.len(), 2);
/// assert_eq!(array.null_count(), 1);
///
/// let batch = RecordBatch::from_payload(array.into_array_ref());
/// assert_eq!(batch.num_rows(), 2);
/// assert_eq!(layout_of(&TypeUrn::parse("std/core/v1/Float64")?), Ok(DataType::Float64));
/// # Ok::<(), TypeUrnError>(())
/// ```
pub mod prelude {
    pub use crate::array::{
        Array, ArrayExt, ArrayRef, BinaryArray, BooleanArray, FixedSizeBinaryArray,
        FixedSizeListArray, Float16Array, Float32Array, Float64Array, Int8Array, Int16Array,
        Int32Array, Int64Array, IntoArrayRef, LargeBinaryArray, LargeStringArray, ListArray,
        NullArray, PrimitiveArray, StringArray, StructArray, TimestampArray, UInt8Array,
        UInt16Array, UInt32Array, UInt64Array,
    };
    pub use crate::buffer::{Bitmap, Buffer, ScalarBuffer};
    pub use crate::builder::{
        ArrayBuilder, BinaryBuilder, BooleanBuilder, BuilderExt, FixedSizeBinaryBuilder,
        FixedSizeListBuilder, Float16Builder, Float32Builder, Float64Builder, Int8Builder,
        Int16Builder, Int32Builder, Int64Builder, LargeBinaryBuilder, LargeStringBuilder,
        ListBuilder, NullBuilder, StringBuilder, StructBuilder, TimestampBuilder, UInt8Builder,
        UInt16Builder, UInt32Builder, UInt64Builder,
    };
    pub use crate::datatype::{DataType, F16, Field, Schema};
    pub use crate::error::{DataError, Result};
    pub use crate::ipc::{IpcError, decode_payload, encode_payload};
    pub use crate::kernel::{OverflowPolicy, cast, concat, filter, take};
    pub use crate::record_batch::RecordBatch;
    pub use crate::tensor::{ImageView, TensorView};
    pub use crate::urn::{TypeUrn, TypeUrnError, layout_of, validate};
    pub use crate::{AstrsMessage, DATA_COLUMN, MAX_PAYLOAD_BYTES, SchemaHash};
}
