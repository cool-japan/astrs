//! Zero-copy `From`/`TryFrom`-shaped conversions between `astrs-data` and
//! arrow-rs 59 (blueprint §6.1), behind the non-default `arrow-interop`
//! feature.
//!
//! `astrs-data` implements the Arrow IPC wire format natively and never needs
//! arrow-rs to talk to another AstRS node (`crate::ipc`, gated by nothing).
//! This module exists for the other direction: embedding AstRS into a
//! pipeline that is already arrow-native — DataFusion, Polars-via-arrow,
//! Parquet, a Python/pyarrow boundary — where the caller wants an
//! `arrow_array::ArrayRef` or `arrow_array::RecordBatch`, not this crate's
//! own types.
//!
//! # Layer map
//!
//! ```text
//!   record_batch   to_arrow_record_batch / from_arrow_record_batch   crate::RecordBatch <-> arrow_array::RecordBatch
//!   array          to_arrow_array / from_arrow_array                 crate::ArrayRef <-> arrow_array::ArrayRef (every P0 family)
//!   datatype       to_arrow_data_type / from_arrow_data_type          crate::DataType <-> arrow_schema::DataType, + Field/Schema
//!   buffer         to_arrow_buffer / from_arrow_buffer                crate::Buffer <-> arrow_buffer::Buffer, + Bitmap <-> Null/BooleanBuffer
//!   error          InteropError                                       this module's own error type (not crate::DataError)
//! ```
//!
//! # Which direction is zero-copy
//!
//! `astrs-data -> arrow-rs` is zero-copy, unconditionally, for every buffer
//! and every array family — see `buffer`'s module docs for the two structural
//! facts that make this true rather than merely usual, and `array::to_arrow`'s
//! for the one case (`Boolean`'s values at a non-byte-aligned bit offset)
//! that needs `arrow_data::ArrayData`'s `offset` field rather than a plain
//! buffer hand-off to stay zero-copy too.
//!
//! `arrow-rs -> astrs-data` always copies. Not a missed optimisation: an
//! arrow-rs `Buffer`'s backing allocation is reachable only through types
//! (`arrow_buffer::Bytes`, `arrow_buffer::alloc::Deallocation`) that are
//! `pub(crate)` to arrow-buffer, so there is no public API to reclaim it —
//! see `buffer`'s module docs for the full argument, including why even the
//! round-trip case (converting an arrow buffer this same module built a
//! moment ago) cannot be special-cased.
//!
//! # A worked example
//!
//! ```
//! use astrs_data::array::{Array, ArrayExt, Int32Array, IntoArrayRef};
//! use astrs_data::interop::{from_arrow_array, to_arrow_array};
//!
//! let array = Int32Array::from_opt_iter([Some(1), None, Some(3)]).into_array_ref();
//! let arrow_array = to_arrow_array(array.as_ref())?;
//! assert_eq!(arrow_array.len(), 3);
//! assert_eq!(arrow_array.null_count(), 1);
//!
//! // And back — a copy on this leg (see the module docs), but the same
//! // logical values.
//! let round_tripped = from_arrow_array(arrow_array.as_ref())?;
//! assert_eq!(round_tripped.as_ref(), array.as_ref());
//! assert_eq!(round_tripped.downcast::<Int32Array>().unwrap().get(0), Some(1));
//! # Ok::<(), astrs_data::interop::InteropError>(())
//! ```

pub mod array;
pub mod buffer;
pub mod datatype;
pub mod error;
pub mod record_batch;

pub use array::{from_arrow_array, to_arrow_array};
pub use buffer::{
    from_arrow_boolean_buffer, from_arrow_buffer, from_arrow_nulls, to_arrow_boolean_buffer,
    to_arrow_buffer, to_arrow_nulls,
};
pub use datatype::{
    from_arrow_data_type, from_arrow_field, from_arrow_fields, from_arrow_schema,
    to_arrow_data_type, to_arrow_field, to_arrow_fields, to_arrow_schema,
};
pub use error::{InteropError, Result as InteropResult};
pub use record_batch::{from_arrow_record_batch, to_arrow_record_batch};
